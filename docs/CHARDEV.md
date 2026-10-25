# Character devices served from ring 3

**Status: design, 2026-10-25. Not built.** It answers the gap
`docs/USB-SERIAL.md` §6.2 names, and the certification consultant's K5
verdict (ledger 4317, B3/B4): "a read/write/poll data path through the
chardev core is a new interface of the V-10 kind and needs its own design".
This is that design. It needs the consultant's verdict before any code.

The goal: any ring-3 driver can serve a character device under `/dev`
(a UART that is not a tty, a JTAG cable, an I2C or SPI bus, CAN, a sensor,
a GPIO chip, `usbfs` nodes for libusb), with Linux's file semantics, and
without a kernel change per class of device. The kernel holds no class
knowledge and names nothing itself. The design also aims for a smaller
attack surface than Linux's equivalent (CUSE, character devices in
userspace) and than our own V-10.

## 1. What exists

The chardev core (`src/kernel/src/interfaces/chardev`, load ring, about 6,200
lines including its self-check) already forwards a program's file
operations on a node to a ring-3 driver. It was built for NVIDIA's `nvrm`
and later serves the Intel GPU's render node:

* A driver asks for a control channel (`ChardevControlCreate`). Its end
  carries no `TRANSFER` or `DUPLICATE`. HELLO lists the minors it serves;
  `ferrix_chardevctl::session` judges it, and the kernel names each node.
* open, ioctl, mmap and release become REQUESTs on the channel. The
  program's thread sleeps killably until `ChardevReply`.
* While a request is outstanding, the driver may copy to and from the
  program's memory: `chardev_copy_in`/`chardev_copy_out`, in 4 KiB chunks
  that each re-check, up to `MAX_COPY` (1 MiB) a call. **Anywhere in the
  program's memory**: that is V-10, accepted for NVIDIA because RM follows
  pointers in its ioctls itself.
* Death of the driver: every open file answers `ENODEV` for good, every
  waiter wakes (N9). A full channel makes the request wait killably; a
  release's room is reserved at open (N8). Request ids are never reused (N6).
* Signals: version 1 abandons; `VERSION_INTERRUPTS` drivers get one
  `INTERRUPT` and the program waits on, killably.
* mmap: the driver answers with its own VMO or an aperture range, mapped
  only after the answer (`MapReply`).
* **What it lacks:** read and write answer `EINVAL`; poll is never ready;
  names are pinned to major 195 (`nvidia<N>`, `nvidiactl`, `nvidia-modeset`)
  and one render node (`ferrix_chardevctl::node`); a control needs `MANAGE`
  on a device with isolated interrupts, which a USB class driver or a
  purely software driver does not have.

So the transport, the lifecycle and the copy machinery exist and are
checked. What is missing is data (read, write), readiness (poll), naming,
who may serve, and above all **confinement**: a generic driver must not get
V-10's whole-memory window.

## 2. Requirements

Linux parity, as a program sees it:

1. open, release, read, write, `llseek`, ioctl, mmap, poll/select/epoll,
   `fsync`/flush, `O_NONBLOCK` (`EAGAIN`), `O_ASYNC`/`F_SETOWN` (`SIGIO`).
2. Many openers per node, each with its own file state; `dup` and `fork`
   share a file, as Linux does.
3. Hot-unplug: when the driver goes, open files answer `ENODEV` (read and
   write may answer `EIO` where Linux drivers do; the driver chooses before
   it goes), poll reports `EPOLLHUP | EPOLLERR`, and the node leaves `/dev`
   before any waiter wakes.
4. Stock programs work unmodified: `i2c-tools`, `spidev_test`, `libgpiod`,
   `candump` (via a char-device bridge), OpenOCD/libftdi/flashrom over
   libusb on `/dev/bus/usb/BBB/DDD`, `minicom` on a non-tty UART.

Ferrix's own rules:

5. Keep the kernel slim (customer, 2026-10-22): no device class, vendor,
   name or ioctl number is known to the kernel. Policy is ring 3's,
   checked by small generic kernel rules.
6. The `max` build: the naming and grant policy is image-time data; no
   run-time setting is added.
7. A driver is not trusted with the memory of the programs calling it,
   beyond what each call hands it (§3.4).
8. Bounded: every queue, cap and walk has a stated bound, and memory is
   charged to the job that caused it (T.EXHAUST).

## 3. Design

### 3.1 Who may serve: a grant from devmgr

devmgr already decides which driver serves which device. It gets one more
power: to hand a driver a **chardev grant**, a native object that is the
right to create one control channel. It is minted by devmgr, which holds
the root grant from boot, and carries the node set the driver may publish
(§3.2). The grant is `TRANSFER` but not `DUPLICATE`, so devmgr can pass it
to exactly one driver process, and it is consumed by
`ChardevControlCreate`. A device handle with `MANAGE` and isolated
interrupts stays a second way in, as today, for GPU drivers.

This removes the device-handle precondition for USB class drivers and
software drivers, without giving them anything else.

### 3.2 Names and numbers: K4''s rule, generalised

Publishing follows the consultant's K4' shape (ledger 4317, (15)-(19)): the
name, number, mode and group come from devmgr's image-time table, and the
kernel checks generic rules only, failing closed, in order:

1. The grant names this node: devmgr minted it for this name and number.
   Otherwise `EPERM`.
2. **The name rule:** a path of at most four components under `/dev`. Each
   component is 1 to 32 bytes of `[a-z0-9_.-]`, not `.` or `..`, and starts
   with a letter or digit. This is wider than `register_block`'s
   letters-and-digits rule, which cannot spell `i2c-1`, `spidev0.0` or
   `bus/usb/001/002`. Otherwise `EINVAL`.
3. No shadowing: not a fixed node (`null`, `tty`, `pts/*`, ...), not a
   published name, not a prefix of one as a file. Otherwise `EEXIST`.
4. The number: a major from Linux's dynamic ranges (234 to 254, 384 to 511)
   or one devmgr's table reserves, never a kernel-served major; the pair not
   in use. Otherwise `EBUSY`.
5. Mode at most 0666, never execute or set-id. Owner root; group from the
   table.

A cap on published nodes per grant and system-wide is stated (proposed: 256
per grant, 4,096 in all). NVIDIA's and Intel's fixed names become entries
in devmgr's table under the same rules; `ferrix_chardevctl::node` keeps
only the rule, not the names.

### 3.3 Operations

New REQUEST kinds beside OPEN, IOCTL, MMAP and RELEASE:

| Op | Carries | Driver answers |
|---|---|---|
| READ | file, offset, length, flags (`O_NONBLOCK`) | bytes (inline or by copy-out), or an errno |
| WRITE | file, offset, length, flags; data inline up to 4 KiB | count taken, or an errno |
| LLSEEK | file, offset, whence | new offset (or the default: the kernel does it if the driver set `SEEK_DEFAULT` at open) |
| FLUSH | file (each `close` of a descriptor) | status |
| FSYNC | file, datasync | status |

Data: a WRITE up to 4 KiB rides in the request, larger ones are copied in
by the driver. A READ answer up to 4 KiB rides in the reply, larger ones are
copied out. Both use the existing chunked copy calls, confined as §3.4
says. A program never waits on more than one request per thread.

### 3.4 Confinement: the driver sees only what the call hands it

This is the part that makes the generic path safe, and the part that goes
beyond Linux. Every outstanding request gets a **window**: the set of
ranges of the program's memory the driver may copy. Copies outside it are
refused `ACCESS_DENIED`, checked per 4 KiB chunk, as N4 already re-checks
liveness.

* **Tier 0, the call's own buffers.**
  * READ and WRITE: exactly `[buf, buf + len)`, out or in.
  * IOCTL: the range `_IOC_SIZE(cmd)` at `arg`, in for `_IOC_WRITE`, out
    for `_IOC_READ`, as the encoding says. An ioctl whose number encodes no
    size gets an empty window, so only its `arg` value reaches the driver.
  * This covers most Linux ioctls, since nearly all are `_IOW`/`_IOR`/`_IOWR`.
* **Tier 1, pointers the program put in its argument.** A driver may widen
  the window only by naming a pointer that lies inside a range it already
  has. It calls `chardev_window_follow(request, at, ptr_width, len_at,
  len_width, way)`. The kernel itself reads the address and the length from
  the program's memory at those offsets, which must lie inside the current
  window, and adds `[address, address + length)` with the given direction.
  * Bounds: depth at most 3, at most 64 ranges, and at most 4 × `MAX_COPY`
    in total per request.
  * The driver chooses which fields to follow (the kernel knows no struct),
    but it can only reach memory the program pointed to.
  * This covers `I2C_RDWR` (arg → `i2c_msg[]` → each `buf`),
    `SPI_IOC_MESSAGE` (`spi_ioc_transfer[]` → `tx_buf`/`rx_buf`),
    `USBDEVFS_CONTROL`/`BULK`, `GPIO_V2_LINE_*`, and most V4L2 and input
    ioctls.
  * *Time of check:* the program can change a pointer after the kernel
    read it. It then has handed the driver a different range of its own
    memory, which it was free to do in the first place. The window only
    ever holds ranges the program named, so no race reaches memory it did
    not offer.
* **Tier 2, undecoded (today's V-10).** Copies anywhere in the program's
  memory while it waits, for drivers whose grant says so: `nvrm`, and Intel
  where it needs it. devmgr's table marks the grant; a generic driver never
  gets it.

The effect: the generic path does not widen V-10, it narrows it. A
compromised UART or I2C driver reads and writes only the buffers its
callers gave it. That is less than any ring-0 Linux driver, and less than
CUSE, whose unrestricted-ioctl retry lets a privileged server fetch any
address in the caller.

### 3.5 Readiness without a round trip

Poll must not cost a channel round trip per `poll()`. So the driver
**posts readiness** per open file: `chardev_ready(control, file, mask)` with
`EPOLLIN | EPOLLOUT | EPOLLPRI | EPOLLERR | EPOLLHUP | EPOLLRDHUP`.

* The kernel keeps the mask. poll, select and epoll read it, level- and
  edge-triggered, and a change wakes their waiters. It is state, not a queue
  (the consultant's coalescing rule, K2 (7)).
* Fast path: a READ or WRITE with `O_NONBLOCK` on a file whose mask lacks
  `EPOLLIN`/`EPOLLOUT` is answered `EAGAIN` by the kernel, with no request.
  A blocking READ is sent; the driver answers it when data comes, or holds
  it. Requests waiting this way count against the per-control bound.
* `O_ASYNC`: the mask's rising edges send `SIGIO` to the file's owner, as
  Linux's `kill_fasync`.
* Driver death sets `EPOLLHUP | EPOLLERR` on every file before waking
  anyone (§3.6).

### 3.6 Lifecycle, failure and bounds

* **Death:** N9 as today, plus K6's order. The nodes are unpublished,
  readiness turns to HUP|ERR, and then waiters wake with `ENODEV`. A
  restarted driver publishes afresh; old files stay dead, as after a Linux
  hot-unplug.
* **Signals:** unchanged (`VERSION_INTERRUPTS`, killable always). A blocking
  READ abandoned by a signal is cancelled with CANCEL to the driver, so a
  late answer is dropped and its bytes are not lost silently: the driver
  learns the read did not happen.
* **Bounds:**
  * Requests per control, unchanged (N8).
  * Outstanding blocking requests per file: 16, then `EAGAIN` for
    non-blocking callers and a wait for blocking ones.
  * Inline data is staged in kernel memory charged to the **calling
    program's job**, not the driver's, at most 4 KiB a request.
  * Windows are at most 64 ranges.
  * Published nodes have the cap in §3.2.
  * All of these are listed in MEMORY-AND-TIMING.
* **No timeouts in the kernel:** like Linux, a stuck driver holds its
  callers killably. devmgr's restart machinery (T0) is the remedy, and it
  sees a driver whose oldest request is older than its policy says.

### 3.7 What stays out of the kernel

* Device classes, ioctl numbers and struct layouts. Tier 1 follows offsets
  the driver gives it.
* Names, numbers, modes and groups: devmgr's table.
* `/sys/class/<class>/...` attributes (K7, pyserial's `list_ports`). A
  later design can publish read-only attribute files from the same grant,
  and is not part of this one.
* Any line discipline. Terminals stay on K1-K6's driver-backed pty, which
  carries sessions and job control; a chardev is never a tty.

## 4. Where the change goes

| Part | File | Ring |
|---|---|---|
| READ/WRITE/LLSEEK/FLUSH/FSYNC requests, readiness, windows, `follow`, the fast path | `src/kernel/src/interfaces/chardev/` | load |
| The generic name rule; node set in the grant; HELLO version 3 | `src/lib/proto/chardevctl` | none |
| Publishing under nested names | `src/kernel/src/fs/devfs.rs` | load |
| The grant object, `ChardevControlCreate`'s second precondition, `chardev_window_follow`, `chardev_ready` | `native-abi` numbers; dispatch in `syscall/native.rs` | **item** for the dispatch lines |
| devmgr's table, minting grants | `src/user/system/native/devmgr` | ring 3 |

If the dispatch can carry the new calls through the existing
"answered by the load ring" path without editing `syscall/native.rs`
(e.g. as flags on `ChardevReply`/`ChardevCopy*`), the design stays out of
the item. Otherwise those lines need L ids reserved first. That is the
consultant's call; the proposal prefers new numbers for clarity.

## 5. Compared with Linux

| | Linux (in-kernel driver) | Linux CUSE | Ferrix (this design) |
|---|---|---|---|
| A driver fault | crashes or corrupts the kernel | kills the server; files `ENODEV` | kills the driver; files `ENODEV`; devmgr restarts it |
| Memory a driver can reach | everything | caller's whole memory (unrestricted ioctl) | only the call's buffers and what they point to (Tier 0/1) |
| poll cost | in-kernel | a round trip per poll | none: posted readiness |
| Nonblocking read with no data | in-kernel | a round trip | none: the kernel answers `EAGAIN` |
| Naming | driver chooses, udev renames | server chooses | devmgr's table, generic kernel checks |
| A tty | yes | no | no (K1-K6 cover ttys) |
| Data copies per read | 1 | 2 (FUSE) | 1 (copy-out from the driver) |

Where Ferrix is still slower: a blocking read is one IPC round trip
(about 1 µs at today's IPC figure). Bulk paths use mmap, as on Linux.

## 6. Evidence the landing must bring

* **Load statements** `chardev-g-1...` in this file, each naming its check.
* **Self-check cases** in `interfaces/chardev/check.rs`, each with a gate
  control that must read `control: FIRED`:
  * a copy outside the Tier 0 window is refused;
  * `follow` from outside the window is refused, and past depth 3 or past
    64 ranges;
  * a Tier 2 copy without the grant mark is refused;
  * the readiness fast path answers `EAGAIN` with no request sent;
  * death sets HUP before any waiter wakes;
  * a name or number refusal per rule of §3.2.
* **A ring-3 test driver** (`chardev-loop`: echo, a pointer-carrying ioctl
  shaped like `I2C_RDWR`, and a readiness toggle), and a gate
  `xtask test-chardev` that drives it from busybox and from a C program
  using poll, epoll, `O_NONBLOCK` and `SIGIO`.
* **A stock program, unmodified:** `i2cdetect`/`i2ctransfer` against a
  ring-3 `i2c-dev` driver over the FPGA I2C target on the DK1 (or a model
  of it in QEMU).
* **Rows `run: PASSED`:** check, test-chardev, the existing NVIDIA and Intel
  chardev gates (Tier 2 unchanged), and test-boot on all three
  architectures.
* **VULNERABILITY-ANALYSIS:** V-10 reworded to Tier 2 only, and a new row
  (V-18) for what Tier 0/1 leaves: a driver reads and writes what its
  callers hand it.

## 7. Order and first users

1. The grant, the name rule and the window machinery, with `chardev-loop`.
   Move NVIDIA's and Intel's names into devmgr's table, behaviour unchanged.
2. READ/WRITE/poll, and the FTDI Blaster's `/tmp/jtag0` moved to
   `/dev/jtag0`.
3. `i2c-dev` and `spidev` drivers for the STM32MP1's I2C5 and SPI5, against
   the FPGA test target (the IRQ-latency probe and the fault-injecting
   SPI/I2C device on the DK1).
4. `usbfs` nodes (`/dev/bus/usb/BBB/DDD`) served by the USB host process
   through the consultant's R3 broker, so libusb, OpenOCD and flashrom run
   unmodified. **Open:** `USBDEVFS_SUBMITURB`/`REAPURB` are asynchronous,
   so the buffer must outlive the ioctl. That needs either a persistent
   window (pinned for the URB's life, bounded per file) or libusb's
   mmap'd-buffer mode (`USBDEVFS` mmap), which keeps Tier 0/1 intact. The
   proposal is mmap'd buffers only, with persistent windows refused.

## 8. Questions for the consultant

1. New native call numbers (touching `syscall/native.rs`, item), or flags
   on existing calls (load only)?
2. Is Tier 1's `follow` acceptable as specified, in particular the time of
   check argument and the bounds (depth 3, 64 ranges, 4 × `MAX_COPY`)?
3. Is the grant object a new native object type (`L.object.*` ids), or
   expressible as a restricted channel end?
4. Should V-10's NVIDIA/Intel nodes move under devmgr's table in the same
   landing, or stay pinned in `ferrix_chardevctl::node` until a later one?
