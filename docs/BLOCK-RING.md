# The block ring: kernel ↔ ring-3 block driver

Version 1. Written by ferrix-61 (stage 11, formerly ferrix-4d), the ring's first
consumer; reviewed and approved with changes by ferrix-d9 (stage 10, formerly
ferrix-8b), who owns the ring, `devmgr` and the driver; identity and naming
decided by the product owner, ferrix-32. `src/lib/proto/blkring` (`ferrix-blkring`)
implements it, host-tested, under Miri and fuzzed. `src/kernel/src/interfaces/block_ring`
is the kernel glue; the driver process is not built yet.

## 1. What this is, and what it is not

`docs/ARCHITECTURE.md` §7: *"The data path is not per-request IPC. Driver and
kernel share a descriptor ring in a VMO and ring a doorbell; requests batch."*

This document specifies that ring for block devices:

* the **control plane** — how the kernel and a driver introduce themselves,
  exchange the shared objects, and part — over the `Channel` §7 gives every
  driver to the subsystem it serves;
* the **data plane** — a submission ring the kernel writes and the driver
  consumes, a completion ring the driver writes and the kernel consumes, and a
  data VMO the payload is copied through;
* the **doorbells** — how each side wakes the other, using stage 9 objects on
  main today.

It is **not** the virtio protocol. The driver speaks virtio-blk to the device
(`src/lib/drivers/virtio::blk`); this is what the kernel speaks to the driver. The shapes
are deliberately similar so one request here becomes one virtio request.

### Constraints (from ferrix-d9, formerly ferrix-8b)

1. v1 copies data through **one shared data VMO** the driver creates and pins
   (`VMO_PIN`, 0x1025) and the kernel maps or reads and writes through. Entries
   carry an offset and length into it, so the device only ever sees pinned
   pages. Zero-copy is later.
2. **Request:** id, op (read/write/flush), sector, count, data offset.
   **Completion:** id, status, bytes done.
3. **Doorbells use stage 9 objects** (§5).
4. **Indices never trust the other side** (§4).

### Consumer side, for orientation

In the kernel, `src/lib/fs/block::Queue` sits in front of the ring:
`Queue::dispatch(now)` yields a merged command → the kernel copies write data
into the data VMO and publishes one submission entry → the driver completes it →
the kernel copies read data out and calls `Queue::complete(token, status)`,
which fans the result back to every request merged into it. The ring's `id` is
the queue's dispatch `Token` value. `src/lib/fs/block` holds no buffers, so the data
VMO allocation (§3.3) belongs to the ring glue, not the queue.

## 2. Objects and handles

| Object | Created by | The other side holds it with | Purpose |
|---|---|---|---|
| Control `Channel` | devmgr / kernel (§7) | — (one endpoint each) | Setup, geometry, shutdown |
| Ring VMO | driver (`vmo_create`) | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | Header + both entry arrays |
| Data VMO | driver (`vmo_create`, then `VMO_PIN`) | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | Payload, copied through |
| Driver port | driver (`port_create`) | kernel: exactly `WRITE \| TRANSFER` | Submission doorbell; also the driver's own interrupt and control packets |
| Kernel completion port | kernel (a real `Port`) | driver: exactly `WRITE` | Completion doorbell |

Rights only shrink (`rights.rs`), so each side hands the other exactly what it
needs.

**Rights at handoff are exact, and checked.** The driver sends the ring and
data VMOs with `READ | WRITE | MAP | TRANSFER` and **no `DUPLICATE`**, and its
port with `WRITE | TRANSFER`. `TRANSFER` is there because it has to be:
`channel_write` takes only a handle that carries it, and a transfer keeps a
handle's rights, so every handle that arrives in a HELLO has it (the first
kernel glue found the rule written without it, and no HELLO could pass). The
kernel **refuses HELLO** if any handle carries more rights than that, or
fewer: the kernel's handle to a driver's object must be one nobody can copy.
The kernel inserts the completion port for the driver with `WRITE` only, since
it places the handle in the driver's table itself rather than sending it.

The ring VMO is not pinned: the device never sees it. Only the data VMO is.

## 3. Memory layout

All multi-byte fields little-endian on every architecture. No padding: every
structure is laid out with explicit offsets, and the tests assert them.

### 3.1 Ring VMO

```
offset  size  field
0       4     magic            b"FXBR"
4       2     version          1
6       2     flags            0 in v1; unknown bits refused
8       4     entries          power of two, 2..=4096; same for both rings
12      4     sub_offset       byte offset of the submission array
16      4     comp_offset      byte offset of the completion array
20      4     sub_tail         WRITTEN BY KERNEL: next submission slot to fill
24      4     sub_head         WRITTEN BY DRIVER: next submission to consume
28      4     comp_tail        WRITTEN BY DRIVER: next completion slot to fill
32      4     comp_head        WRITTEN BY KERNEL: next completion to consume
36      4     sub_want_bell    WRITTEN BY DRIVER: 1 = ring me after publishing
40      4     comp_want_bell   WRITTEN BY KERNEL: 1 = ring me after publishing
44      20    reserved         zero
64      ...   submission array: entries × 32 bytes, at sub_offset
...     ...   completion array: entries × 24 bytes, at comp_offset
```

`magic`, `version`, `flags`, `entries`, `sub_offset` and `comp_offset` are
written by the driver before HELLO. The kernel reads them **once**, validates
them (§4.1), and keeps its own copies; it never reads them again.

Every other field has exactly one writer, as marked. **Each side keeps its own
indices and its own want-bell flag privately and only ever writes them to the
ring. It never re-reads its own fields from shared memory**, because the peer
can scribble on them: a driver that overwrites `comp_head` must not be able to
move the kernel's idea of where it has read up to.

Indices are free-running `u32`s: slot = index & (entries − 1). A ring holds
`tail − head` (mod 2³²) entries.

### 3.2 Entries

**Submission** (32 bytes; written by the kernel, read by the driver):

```
0   8  id           kernel's choice; unique among outstanding submissions
8   8  sector       first logical sector
16  8  data_offset  bytes into the data VMO
24  4  count        sectors; 0 for FLUSH
28  1  op           1 READ, 2 WRITE, 3 FLUSH
29  1  flags        bit 0 FUA (WRITE only, and only if HELLO announced it)
30  2  reserved     zero
```

Payload length = `count × block_size` (§6.1), in `[data_offset,
data_offset + length)` of the data VMO.

**Completion** (24 bytes; written by the driver, read by the kernel):

```
0   8  id          of the submission this completes
8   8  bytes_done  ≤ the submission's payload length; 0 for FLUSH
16  4  status      0 OK, 1 IOERR, 2 UNSUPPORTED, 3 REFUSED (failed driver
                   validation), 4 READ_ONLY
20  4  device_ticks  how long the device held the request, in the ticks of
                   the processor's free-running counter as the driver read
                   it; 0 when the driver did not measure
```

`device_ticks` (2026-09-27, the seam's measurement in `docs/OPAQUE-KERNEL.md`)
was a reserved zero before, which no kernel checked, so an older driver's
completions read as "not measured". The kernel acts on nothing in it: it
sums the ticks for the `seam` boot line, converted with its own clock's rate
only when ring 3 reads the same counter (`arch::vdso_can_read_counter`).

### 3.3 Data VMO

One VMO, created and pinned by the driver — it holds the device handle
`VMO_PIN` needs and knows `max_sectors` — with its size announced in HELLO
(§6.1). It must hold at least one request of `max_sectors × block_size` bytes;
a HELLO announcing less is refused as an unusable device (reason 6). The kernel reaches its pages by mapping them or by `Vmo` read and
write; that is the kernel glue's choice, not the protocol's.

**The kernel allocates regions of it.** It is the one issuing requests, so it
chooses each `data_offset`, never reuses a region before that region's
completion, copies write payloads in before publishing, and copies read
payloads out after completing. The driver never allocates in it; it validates
that each submission's region lies inside the VMO.

**No region alignment.** The pin query (0x1026) gives one device address per
page, and those addresses are not contiguous. The driver splits a region at
page boundaries into virtio data descriptors, each at its page's device address
plus the offset within the page. A request therefore takes at most
(pages spanned + 2) descriptors (header, data pages, status), and the driver
computes HELLO's `max_sectors` so that this fits both the device's `seg_max` and
its queue size.

## 4. Trust and validation

Neither side trusts the other's writes. Each copies a shared field once into a
local before using it, so a peer changing it mid-use cannot make a check and a
use disagree — and, per §3.1, neither ever reads back its own fields.

### 4.1 At setup (kernel, reading the ring header once)

* `magic` and `version` known (v1 refuses any other version); `flags` has no
  unknown bits.
* `entries` a power of two in range.
* Both arrays lie inside the ring VMO and overlap neither each other nor the
  header.
* The initial driver-written indices and want-bell flag are zero.

Any failure: the kernel sends REFUSED (§6.2) and never maps further.

### 4.2 Every read of the other side's index

* Read the peer's index once. Let `pending = peer_tail − own_head` (mod 2³²),
  with `own_head` from this side's private copy.
* **`pending > entries` is corruption.** The response depends on who detects
  it. The kernel fails every outstanding request with EIO and treats the
  driver as dead (§6.3). A driver detecting it resets the device and stops.
* **A peer index that moves backwards** relative to the last value this side
  observed is the same corruption.

### 4.3 Every entry consumed

Driver, per submission:
* op known; reserved and unknown flag bits zero; FUA only with WRITE and only
  if announced;
* `count > 0` except FLUSH, and `count ≤ max_sectors`;
* `sector + count ≤ capacity` without overflow;
* payload region inside the data VMO without overflow.

Any failure → a completion with `REFUSED`; the device never sees the request.

Kernel, per completion:
* `id` names a submission that is outstanding. **An unknown id, or an id that
  was already completed, is corruption** (§4.2's response).
* `status` known.
* `bytes_done ≤` the submission's payload length. A WRITE or FLUSH completing
  with status OK but `bytes_done` short of its length is treated as IOERR.

The kernel copies read data out of the data VMO **once, after completion,
straight into the caller's buffer**, and keeps the region out of use until
that copy is made. It never validates payload contents — btrfs checksums
them, in the caller's buffer — so a driver changing bytes after the copy
changes nothing the kernel acts on, and one changing them before it only
changes what the checksum is run over. The copy is volatile loads, a machine
word at a time (§3.1's shared fields are the same: every index and want-bell
flag is one whole `u32` access, never bytes).

The kernel reads every field of a completion once, into its own memory,
checks that copy and acts on it; it never reads a field again after checking
it. Stage 10's `reread` boot line (`block_ring::reread_check`) holds that
against a driver that rewrites each field of a posted completion — tail, id,
`bytes_done`, status — the instant after the kernel first reads it, each to a
value that would also have passed the checks.

## 5. Doorbells

Both doorbells are **port packets** (`port_queue`, 0x1019), `PACKET_USER`, on a
port the receiver created and the sender holds with `WRITE` only.

| Direction | Port | `key` | `data[0]` | `data[1]` |
|---|---|---|---|---|
| kernel → driver (new submissions) | driver port | `BELL_SUBMIT` = 1 | `sub_tail` at ring time | 0 |
| driver → kernel (new completions) | kernel completion port | `BELL_COMPLETE` = 2 | `comp_tail` at ring time | 0 |

`data[0]` is a hint only; the receiver always re-reads the index from the ring.

**Bells are hints, and any number counts as one.** User packets are bounded:
`PORT_CAPACITY` is 1024 per port, and `queue_user` answers Full beyond that.
Consequences:
* A sender whose `port_queue` answers Full treats it as **"already rung"**,
  never as an error. Both sides do.
* The kernel's completion receiver is a normal `Port`, and a kernel task blocks
  on `Port::waiters()` and `take()`s packets. A user `port_queue` wakes it at
  once. It drains every queued bell and then re-reads the ring once, so a
  driver flooding bells only delays its own completions.

The driver's port also receives its device interrupt packets
(`interrupt_bind`, `PACKET_INTERRUPT`) and a signal packet for the control
channel (`object_wait_async` for `READABLE | PEER_CLOSED`), so one `port_wait`
loop services everything. Keys 1 and 2 are reserved for the ring; the driver
chooses its interrupt and control keys outside them.

### Why port packets, not channel messages

A channel message is copied and queued per send and carries a byte payload a
doorbell does not need; a packet is fixed-size and cheap. The control channel
stays free for control.

### 5.1 Coalescing without lost wake-ups

A doorbell per entry would reintroduce per-request IPC. Each ring has a
*want-bell* flag, written only by the ring's consumer.

The consumer, before sleeping:
1. writes `want_bell = 1`;
2. re-reads the producer's tail; if anything is pending, writes
   `want_bell = 0` and processes instead of sleeping;
3. otherwise `port_wait`s.

The consumer, on waking: writes `want_bell = 0`, then drains.

The producer, after publishing one or more entries — that is, after writing the
new tail — reads `want_bell`; if it is 1, queues the packet (Full counts as
rung).

The producer publishes before reading the flag, and the consumer sets the flag
before re-reading the tail, so one of them always sees the other: no lost
wake-up. A hostile producer can still queue bells freely, which by the rules
above costs only itself.

## 6. Control plane (on the control Channel)

Messages are fixed little-endian structures: the first 4 bytes are a message
type, the next 4 its length. Handles ride in the message's handle array.

### 6.1 driver → kernel: HELLO (type 1)

```
0   4  type = 1
4   4  length
8   2  ring version (1)
10  2  queues            ring pairs; 1 in v1 (reserved for multi-queue)
12  4  block_size        512 × 2^n, from virtio BLK_SIZE (512 if not negotiated)
16  8  capacity          in block_size sectors
24  4  max_sectors       per request; fits seg_max and the queue size (§3.3)
28  4  device_flags      bit 0 READ_ONLY, bit 1 FLUSH, bit 2 FUA
32  8  data_vmo_size     bytes
40  4  location          the device's PCI address: segment in bits 31:16,
                         bus in 15:8, devfn in 7:0; or, for a device tree
                         node (an STM32MP15's SD card), 0xFFFF_FFFF,
                         `DEVICE_NOT_PCI`, which names one such disk
44  20 serial            virtio-blk VIRTIO_BLK_T_GET_ID bytes; all zero if the
                         device does not answer
64  8  name              the node name devmgr chose, ASCII, NUL-padded:
                         `vd` and 1 to 3 lowercase letters (vda … vdzzz)
handles: [ring VMO (READ|WRITE|MAP), data VMO (READ|WRITE|MAP),
          driver port (WRITE)]
```

`length` is 72. `device_flags` are derived from the **negotiated** virtio
features (RO, FLUSH, and write-cache or FUA support as negotiated), never from
the merely offered ones. `queues` other than 1 is refused in v1.

**Naming and identity** (decided by ferrix-32, fields by ferrix-d9). The kernel
creates the block device node in devfs when it accepts HELLO, under `name`.
**The kernel, not HELLO, chooses the numbers** — a driver that picked its own
minor could collide with another's: the major is the kernel's one virtio-blk
major, and the minor is `index × 16` for the whole disk, with `index` computed
from the name as Linux does (`vda` 0, `vdz` 25, `vdaa` 26), leaving the 15
minors after it for partitions. So `mount -t btrfs /dev/vda /mnt` reads as on
Linux and `/proc/partitions` lists the disk. devmgr
owns the naming policy: in stage 11 it starts block drivers in PCI-address
order and names them `vda`, `vdb`, … in that order, so names are stable on a
given machine. The kernel keeps `location` and `serial` on the block device for
a later `/dev/disk/by-path` and `/dev/disk/by-id`; stage 11 exposes neither.
HELLO is **refused** when:

* `name` is not `vd` followed by 1 to 3 lowercase letters and then only NUL
  bytes, or names a node already published;
* `location` names a device another accepted driver already serves — the
  kernel's check that two drivers cannot claim one disk.

A device tree node has no PCI address, so its disk is named by
`DEVICE_NOT_PCI`, and that one word names one disk: `block_ring_create`
answers `ALREADY_BOUND` for a second tree node while another holds the word
-- its ring served, or its disk parked for its next driver (§6.3) -- since
parking and the HELLO check key on the word alone. Any other node that is
not a PCI function (a `virtio,mmio` transport) gets `INVALID_ARGS`.
`interfaces/block_ring/tree_check.rs` checks both at every boot.

### 6.2 kernel → driver: READY (type 2) or REFUSED (type 3)

READY carries `[kernel completion port (WRITE)]`. After READY the driver may
start consuming submissions. REFUSED carries a status and no handles; the
driver resets the device and exits. The kernel sends REFUSED for:
* a version mismatch;
* `queues ≠ 1`;
* any §4.1 failure;
* a handle whose rights differ from §2's.


**Refusal reasons for identity** (ferrix-blkring's numbering; 1–6 are the
header, rights and device refusals above): 7 `Name` — the name is not `vd` and
1 to 3 lowercase letters followed only by NUL; 8 `NameInUse` — a node of that
name is already published; 9 `LocationInUse` — another accepted driver already
serves that location; 10 `WrongLocation` — `location` is not the PCI address
of the device node the ring was created for, or `DEVICE_NOT_PCI` for a device tree
node's ring. 7 is checked by `ferrix-blkring`'s
pure validation; 10, 8 and 9 need what only the kernel knows — which node the
ring is bound to, and the registry — so the ring glue reports them. 8 is what
the devfs block registry's `NameInUse` refusal becomes.

**When a HELLO has several faults**, the first found in this order is
reported: version, queues, handle rights, device fields, name, then the glue's
checks: `location` is the ring's own device, then the registry. A driver fixing faults one at a time therefore sees them in
a stable order.

**Numbers.** The whole-disk minor is `index × 16`, and `vdzzz` has index
18277, so minors need at least 19 bits; the kernel keeps Linux's 20. `location`
and `serial` are carried as given: stage 11 validates neither beyond size.

### 6.3 Shutdown

* **Orderly.** The kernel sends STOP (type 4). The driver stops taking
  submissions, completes or fails what it holds, resets the device, and
  replies STOPPED (type 5). Only after STOPPED does the kernel unmap the VMOs
  and fail anything still outstanding with EIO.
* **The driver dies.** The driver's process ending (`TERMINATED`) or the
  control channel's `PEER_CLOSED` is an implicit STOPPED, **without** any
  guarantee that the device was reset. The kernel stops using the ring but
  does **not** fail what was outstanding: since 2026-09-26 (the customer's
  choice, T0) the disk is *parked*. The commands the driver held go back on
  the disk's queue in their epochs, so barrier order holds, its node stays
  published, and callers keep waiting. The next ring made for the same PCI
  location whose HELLO describes the same disk under the same name takes the
  parked disk up and dispatches what waited, so a filesystem mounted on it
  sees a slow request, not an error, when devmgr starts the driver again. A
  request is idempotent at this layer, so one the dead driver had already
  done is only done twice. A disk parked for 30 seconds answers new requests
  with EIO at once, and each waiting request keeps its own 30-second
  patience. A HELLO describing another disk on that location ends the parked
  one first, failing what waited with EIO. **It must not free the data
  VMO's pinned pages** until the device is known quiesced: after an unpin on an
  untranslated domain — which is every domain today — the device can still DMA
  to those frames. The pages stay held (leaked) until devmgr has reset the
  device, or a translated domain has unmapped them. The kernel's own copies are
  already safe. (ferrix-d9 documents the same rule on `Domain::unpin`.)
* **A driver never faults on a file mapping of its own disk** (decision of
  2026-09-13, recorded in `docs/BACKLOG.md`). A page fault on such a mapping
  fills through this ring, and the only driver that could complete that fill is
  the one waiting on the fault: it deadlocks. So the driver's image and every
  mapping it touches come from the initramfs, tmpfs, anonymous memory, the ring
  VMO or the pinned data VMO, or are fully committed before any pivot onto
  btrfs. **devmgr enforces this when it starts the driver**; it is not left to
  a comment. VMO_PIN's frames are committed by pinning, so the data VMO is
  always safe.
* **Reset before release, owned by devmgr** (the product owner's decision, before the
  driver lands). When a driver's Job is killed or the driver ends, **devmgr
  resets its device before any pinned frame is freed**. devmgr learns of the
  end through process observers on a port (ferrix-4b, formerly ferrix-2a).
  * Until that reset, the data VMO's pinned pages stay held — leaked by design
    — and the kernel prints a console line saying so.
  * After devmgr confirms the reset, the pins may be released and the data VMO
    freed. If devmgr cannot reset the device, they are never freed.
  * The kernel's side does not wait for the reset: a parked disk's requests
    wait for the next driver, whose bring-up resets the device before its
    HELLO, and are dispatched to it only then.

### 6.4 devmgr → driver: START (type 6)

The first and only message on a driver's bootstrap channel — `Handle(1)`,
which `ferrix-rt`'s `Bootstrap` adopts — sent by whoever started the driver:
`devmgr`, or the boot check in its stead. A driver handed anything else first
exits.

```
0   4  type = 6
4   4  length = 92
8   16 common       Block: virtio's common configuration
24  16 notify       Block: the notification area
40  16 isr          Block: interrupt status
56  16 device       Block: device-specific configuration; all zero if none
72  4  notify_off_multiplier
76  2  msix_table_size   entries; 0 for a device with a line only
78  2  pci_device_id     0x1042 modern, 0x1001 transitional virtio-blk
80  4  location          as HELLO carries it
84  8  name              the node name chosen for the disk, as HELLO carries it
handles: [device (TRANSFER | MANAGE), control channel (TRANSFER | READ | WRITE | WAIT)]

a Block:
0   8  phys      page-aligned physical start of the pages holding the block,
                 inside one of the device's apertures
8   4  offset    the block's first byte, from phys
12  4  length    bytes
```

Ring 3 cannot walk configuration space, so START carries where each virtio
register block lies, in the form `io_mapping_create` takes: the driver maps
`[phys, phys + round_up(offset + length))` and adds `offset`. The kernel
keeps these on the device node from enumeration and reports them through
`device_info` (0x1049), which is where `devmgr` reads them; the boot check
builds the same START from the node directly. The handles carry exactly the
rights shown, checked as HELLO's are (`Start::validate`). The driver carries
`location` and `name` back in HELLO unchanged.

## 7. Mapping to what exists

* `src/lib/fs/block` → one submission per `Dispatch`; `Token::raw()` is the `id`;
  the queue's `Completion` is built from the ring's status.
* `src/lib/drivers/virtio::blk` and `src/lib/drivers/block/virtio-blk` (in progress) → the driver turns one
  submission into one virtio-blk request chain whose data descriptors are the
  region's pages, split at page boundaries, by device address from the pin
  query.
* `src/lib/fs/btrfs-vfs` → reads through a `Device` handle over `src/lib/fs/block`.

## 8. Implementation: `ferrix-blkring`

A `src/lib/` crate — no_std, forbid(unsafe) — with:

* the layouts, with offset tests;
* `KernelSide` and `DriverSide` over a `RingMemory` trait (as
  `src/lib/drivers/virtio::QueueMemory` does). Each keeps its own indices privately,
  writes only its own fields, and validates every read of the other's;
* the want-bell protocol as methods that return "ring the bell now" rather
  than doing I/O, so the kernel and the driver glue choose how to ring, and
  both treat Full as rung;
* HELLO, READY, REFUSED, STOP and STOPPED encoding and decoding, including the
  rights and version checks as pure functions over the values the glue reads;
* tests stepping both sides against each other, and a fuzz target in which one
  side is the fuzzer. The other side must never panic, loop, read outside the
  VMO, re-read its own fields, or accept an id twice.

The kernel glue (ports, VMO mapping, the data VMO allocator, the leak-until-
quiesced rule) and the driver process glue stay with ferrix-d9.

## 9. Review decisions (ferrix-d9, formerly ferrix-8b)

* **Q1 flooding** — ports are bounded (`PORT_CAPACITY` 1024). Full counts as
  rung, and the kernel receiver treats any number of bells as one (§5).
* **Q2 kernel port** — a real `Port`, with a kernel task on `Port::waiters()`
  (§5).
* **Q3 data VMO** — the driver creates and pins it (§3.3).
* **Q4 alignment** — no `region_align`; regions split at page boundaries,
  pages spanned + 2 descriptors at most (§3.3).
* **Q5 multi-queue** — `queues` u16 reserved in HELLO, 1 in v1 (§6.1).
* **Q6 zero-copy** — later, using the spare flag bits.
* **Q7 versioning** — refuse on mismatch in v1.
* **Changes adopted:** private indices, never re-read (§3.1, §4); a duplicate
  completion id is corruption (§4.3); exact handle rights with HELLO refused
  otherwise (§2, §6.2); pinned pages held until quiesced on driver death
  (§6.3); FUA and FLUSH flags from negotiated features (§6.1).
