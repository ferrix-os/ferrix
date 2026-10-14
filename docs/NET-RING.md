# The net ring

The memory the kernel shares with a ring-3 network driver, and the rules each
side keeps over it. `src/lib/proto/netring` is this document in code; where the two
differ, this document is wrong.

`docs/ARCHITECTURE.md` §7 runs drivers in user processes and says the data path
to them *"is not per-request IPC. Driver and kernel share a descriptor ring in a
VMO and ring a doorbell; requests batch."* `docs/BLOCK-RING.md` is that ring for
disks. This is the one for network interfaces.

## 1. Why it is not the block ring with a different entry

A block request carries anything from a sector to a megabyte, so the block ring
has an allocator: the kernel picks a `data_offset` for every submission and must
not reuse a region before that region's completion. A frame is bounded by the
interface's MTU, so this ring has none.

The data VMO is `entries` slots of `slot_bytes`; slot *n* lives at
`n * slot_bytes`; a submission names its slot. That removes the whole
region-allocation half of the protocol, and with it the class of bug where a
region is reused early — which on an untranslated IOMMU domain, which is every
domain today, is a device writing into somebody else's packet.

What the two rings share is the index discipline: private indices, checked reads
of the peer's, and the want-bell handshake. That is written twice rather than
shared, which is a debt `docs/BACKLOG.md` carries with its reason.

## 2. Objects and the rights they carry

| Object | Created by | The other side holds it with | Purpose |
|---|---|---|---|
| Control channel | devmgr, or the kernel's own check | one endpoint each | setup, the interface's description, shutdown |
| Ring VMO | driver | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | header and both entry arrays |
| Data VMO | driver, pinned | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | the slots |
| Driver port | driver | kernel: exactly `WRITE \| TRANSFER` | submission doorbell |
| Kernel completion port | kernel | driver: exactly `WRITE` | completion doorbell |

Rights at handoff are **exact**, for the block ring's reason: a `DUPLICATE` on a
VMO would let the kernel's handle be copied. A handle a process sends carries
`TRANSFER`, because only a transferable handle can be sent; the completion port
the kernel sends back does not, since the kernel places it in the driver's table
itself.

## 3. The ring VMO

Little-endian on every architecture, no padding.

```text
0   magic "FXNR"     4   version 1        6   flags, 0 in v1
8   entries          12  slot_bytes
16  sub_offset       20  comp_offset
24  sub_tail         written by the kernel
28  sub_head         written by the driver
32  comp_tail        written by the driver
36  comp_head        written by the kernel
40  sub_want_bell    written by the driver
44  comp_want_bell   written by the kernel
48  reserved, zero, to 64

at sub_offset:  entries x 16-byte submissions
                slot 0, length 4, op 8, flags 9, reserved 10
at comp_offset: entries x 16-byte completions
                slot 0, length 4, status 8, reserved 12
```

`entries` is a power of two in `2..=4096`. `slot_bytes` is a multiple of 64 in
`64..=65536`; 64 so that a slot begins on a cache line and two slots are never
in one.

The arrays go immediately after the header, submissions first. A peer does not
get to choose where: letting it would mean checking two ranges against each
other, against the header and against the ring's end on every attach, and fixing
them makes that one comparison and takes nothing away, since both sides link
`src/lib/proto/netring`.

The driver writes the first six fields before HELLO. The kernel reads them once,
in `KernelSide::attach`, and keeps its own copies. Every other field has the one
writer marked above. Indices are free-running `u32`s; the entry for an index is
at `index & (entries - 1)`.

Each index and want-bell word is read and written in **one aligned access** on
both sides, never a byte at a time. Composed from bytes, a tail stepping from
`0x00ff` to `0x0100` is read as `0x01ff` or written through `0x0000`, and the
reader reports a ring run ahead or backwards and ends it. `RingMemory` therefore
does not provide its `u16` and `u32` accessors; every implementation gives them,
and the driver's and the kernel's are single volatile loads and stores. Until
2026-10-15 both inherited byte-wise ones, and under a long download the
virtio-net driver stopped on a "corrupt" ring about every 35 minutes.

An emulator runs one store at a time and cannot show a torn read, so the
kernel's side is shown by its object code. In release kernels built from
d497b8aec, the aligned path of `Pages::read_u32` and `Pages::write_u32` (each
kept out of line) is one 32-bit access to the frame: `movl (%rax,%rsi),%eax` and
`movl %edx,(%rax,%rsi)` on x86-64, `ldr w0,[x8,x9]` and `str w2,[x8,x9]` on
AArch64, `ldr r0,[r0,r1]` and `str r2,[r0,r1]` on ARMv7-A, each naturally
aligned and so single-copy atomic. Their 16-bit (`movzwl`/`movw`, `ldrh`/`strh`)
and byte accesses are the fallback for an unaligned offset only, which the
netring host test `every_shared_index_is_aligned_to_its_size` keeps out of
every honest ring (`llvm-objdump -d` of the two symbols; the certification
consultant's condition 2, ledger line 1141).

## 4. The two operations

A submission is one of:

* **`Transmit`** — the slot holds a frame of `length` bytes. Put it on the wire.
* **`Receive`** — the slot is empty. Fill it with the next frame that arrives.

A completion answers one submission, names its slot, and says how many bytes the
slot holds now: what went out, or what came in. Its status is `Ok`, `Failed`
(the device refused it, or the link is down) or `Abandoned` (the driver gave the
slot back without carrying anything, which is what a reset does to what it
held).

**The kernel owns every slot.** It takes a free one, uses it, and gets it back
with the completion. The driver never chooses a slot and never frees one: it is
the untrusted half, and the less of the protocol it decides the less a broken
one can get wrong.

## 5. Trust

Neither side trusts the other's writes.

* **Private indices.** Each side keeps its own head, tail and want-bell flag in
  its own memory and only ever *writes* them to the ring, so a peer scribbling
  on them moves nothing.
* **Every read of a peer's index is checked.** A tail more than `entries` ahead
  of this side's head, or an index behind the last value this side accepted, is
  corruption.
* **Every entry is checked when it is read.** A slot at or past `entries`, a
  length past `slot_bytes`, an operation or a status the protocol has not, or a
  completion for a slot nobody submitted: each is corruption.

Corruption is **terminal** for the side that sees it. It latches, and every
later call reports the same thing. The kernel takes the interface down and
treats the driver as dead; a driver resets its device and stops.

## 6. Doorbells

Both are `PACKET_USER` port packets, on a port the receiver made and the sender
holds with `WRITE` only. Key 1 is the kernel's bell on the driver's port, key 2
the driver's on the kernel's completion port.

A side that is about to sleep sets its want-bell flag, barriers, and looks once
more; if it finds work it clears the flag and does not sleep. A side that
publishes writes its tail, barriers, and reads the peer's want-bell flag; if it
is set it rings. That ordering is the whole of the lost-wake-up argument, and it
only holds if `RingMemory::barrier` is a real ordering fence.

Ports are bounded, so a queue can be refused for being full. A bell is a hint
and any number of them counts as one, so a refusal for being full means a bell
is already waiting: the call *rang*.

A side that sleeps without setting the flag is never rung. It is worth saying
plainly, because the first ring-3 driver did exactly that and the result reads
as a working interface: the driver still woke on its device's interrupt, so it
received frames and answered none of them, and every frame the kernel gave it
sat in the ring. Setting the flag is not an optimisation to be skipped when a
side is in a hurry -- it is the only thing that makes the other side ring.

## 7. The control channel

1. Whoever starts the driver sends **START**: which device, and where its
   register blocks are.
2. The driver brings the device up, makes its VMOs and its port, and sends
   **HELLO** with them and with the interface's name, hardware address, MTU and
   flags.
3. The kernel answers **READY**, with its completion port, or **REFUSED** with
   the first reason that applies, in this order: version (1), entries (2), slot
   size (3), handles (4), a malformed message (5), a ring too small (6), a data
   VMO too small (7), the name (8), the MTU (9), and last the registry — a name
   already up (10) or a device already served (11). A refusal closes the
   kernel's end.
4. The driver sends **LINK** when the link comes up or goes down.
5. **STOP** asks the driver to stop taking submissions, finish or abandon what
   it holds, reset its device and answer **STOPPED**. A driver that dies is an
   implicit STOPPED *without* the promise that its device was reset.
6. A ring the **kernel** ends -- because it found the ring corrupt -- ends with
   no STOP: the kernel parks the interface and closes its end of the channel.
   The driver must take `PEER_CLOSED` as the end, reset its device and exit
   with a failure (the virtio-net driver's status 9), so that whoever started
   it starts it again and the new HELLO takes the parked interface up. A
   driver that waits on the closed channel is woken at once, every time: the
   first virtio-net driver did that, and spun on one processor with its
   network dead and nothing restarting it.

However a ring ends, every outstanding slot is abandoned at once and the data
VMO's pinned pages are **held until devmgr confirms the device reset**. On an
untranslated domain a device that was not reset can still write to frames after
they are unpinned, and not even an orderly STOPPED proves the reset — it is the
untrusted driver's word. Until devmgr confirms, the pages stay held, leaked by
design, and the kernel says so.

However a ring ends, its **interface is parked, not removed**: the carrier goes,
frames the stack would send through it are dropped, and its index, name,
addresses and routes stay. The next ring made for the same device whose HELLO
names the same interface and hardware address takes it up again under the same
index, so a driver started again brings `eth0` back as it was, and a socket
bound to its address or a route through it sees only the carrier come back. A
HELLO naming something else on that device replaces the parked interface. The
device's claim is released when the ring's task ends, and a quiesce waits for
that (`src/kernel/src/claim.rs`).

## 8. What is not here

Ports, mappings, the pinned pages and the device. Those are the kernel's glue
and the driver process's business. `src/lib/proto/netring` is the bytes and the
arithmetic, so that `cargo test`, Miri and a fuzzer can reach all of it.
