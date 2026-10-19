# Written ahead of their stage

The rule at the bottom of this file — anything expressible as a pure function of
bytes goes to `src/lib/` *before* it is called from the kernel — has a consequence
worth stating plainly, because otherwise the tree looks further along than it is:
several crates for stages that had not started were already written and tested.

**Where it stands (2026-09-23).** Every stage the table names has come, and
every crate in it is used — most by the kernel, `cpio` through `src/lib/fs/common/vfs`,
and `virtio-gpu`, `virtio-net` and `netserve` by their ring-3 drivers — so
the table is now a record of what was written early. Its test counts are
brought up to date (last on 2026-09-30, from `cargo test -- --list` over
each crate's library and integration tests); the prose is as it was
written.

They are parsers and data structures, not subsystems. None of them counted
towards the stage that would consume it, and when this was written most were
still unreachable from the kernel. Four were the exception, which is what the rule was for:
`src/lib/platform/acpi` as of stage 3 — the MADT walk the interrupt controller needed was
already written, tested and fuzz-shaped before a line of controller code
existed — `src/lib/platform/fdt`, which the ARMv7-A port reached from stage 1 because that
machine has no ACPI to read, and, as of stage 2, `src/lib/kernel/vma` and `src/lib/kernel/sync`,
whose `AddressSpace` and `IrqSpinLock` the vmap arena is built on, with
`IrqControl` implemented over each architecture's interrupt mask. Being
reached early is not the same as their stage being done: the arena uses the
interval tree as an allocator of kernel ranges, not as a process's address
space. `src/lib/kernel/sync`'s stage has now come — stage 4's exit test is four
processors contending for one of its ticket locks.

What they buy is that the stage in question begins with its byte-handling
already fuzz-shaped, host-testable and argued about, rather than being written
at three in the morning against a machine that reboots on a mistake.

| Crate | Written for | Tests |
|---|---|---|
| `src/lib/platform/acpi` | 3, 10 — RSDP, XSDT/RSDT, MADT, FADT fixed fields, GTDT, HPET, MCFG, GIC MSI frames, DMAR, IORT, and the HPET block's capability register with the arithmetic a 32-bit counter needs. No AML, and there will be none. Has its fuzz target. | 83 |
| `src/lib/platform/fdt` | Reached at 1 on ARMv7-A — the console, the GIC, the timer's interrupt and the PSCI conduit come from it there, and nothing else describes that machine. Reached at 10 for PCI host bridges `virtio,mmio` devices and `GICv2m` frames; stage 10 is still the rest of it. Has its fuzz target. | 93 |
| `src/lib/kernel/sync` | Reached at 4 — `SpinLock` and `IrqSpinLock` guard every shared kernel structure and carry the contended counter; `RwSpinLock` is still waiting. Fair by construction, because an unfair lock on a starved core is a stage-14 latency bug nobody will find. | 30 |
| `src/lib/kernel/vma` | 6 — backs the vmap arena, and since stage 6 every process's address space. The VMA interval tree and the three calls that reshape it (`mmap MAP_FIXED`, `munmap`, `mprotect`). | 72 |
| `src/lib/proto/linux-abi` | 7 — syscall numbers, `errno`, `repr(C)` layouts, and which identification register fields grant each Arm `AT_HWCAP` bit. Constants and pure functions of them. Three number tables, one of them 32-bit. The socket numbers and address layouts `AF_UNIX`, IPv4, IPv6 and netlink use, and the fixed headers of the routing messages, from a probe compiled against the UAPI headers. Has its fuzz target since 2026-09-23. Reached early for stage 17: the DRM/KMS ioctls, capabilities and structure layouts `/dev/dri/card0` answers, checked line by line against `probe/drm.c`'s output at both widths; and the evdev ioctls, codes and layouts `/dev/input/eventN` answers, against `probe/input.c`'s; and, for stage 19, the `virtgpu` ioctls, against `probe/virtgpu.c`'s. | 146 |
| `src/lib/kernel/ustack` | 7 — the initial process stack `execve` hands a program: argv, envp and the auxiliary vector, at both pointer widths. Has its fuzz target and its Miri step already. | 22 |
| `src/lib/fs/common/cpio` | 8 — the "newc" reader an initramfs is unpacked from. Borrows, copies nothing, allocates nothing. Has its fuzz target. | 45 |
| `src/lib/fs/common/vfs` | 8 — dentries, mounts, the path walk, open file descriptions, descriptor tables, tmpfs over a page store, initramfs unpacking. Written at the start of its stage rather than ahead of it. Has its fuzz target and its Miri step already. | 128 |
| `src/lib/fs/common/procfs` | Reached at 8 — the text of `/proc`: the `maps` line padded to its name column at both pointer widths, `meminfo`, `status`, `stat` and `mounts`, pinned byte for byte against lines a real Linux printed, and the `maps` parser the kernel's boot check reads its own output back with. No fuzz target: it arranges the kernel's own numbers rather than parsing a stranger's bytes. | 36 |
| `src/lib/drivers/virtio` | 10 — the split virtqueue as logic over an abstract shared memory, the PCI transport's status protocol, feature negotiation and queue activation, and each device class's own protocol: virtio-blk's in `blk`, virtio-net's in `net`, and, reached early for stage 17, virtio-gpu's 2D control commands and responses in `gpu`, checked against QEMU 9.2.4's header, and virtio-input's configuration queries and events in `input`, checked against Linux 6.8's header and QEMU 9.2.4's devices; both fuzzed. Reached at 10 by the boot check's virtio-rng driver. | 180 |
| `src/lib/platform/pci` | 10 — configuration space: ECAM geometry, headers, BAR decoding and sizing, both capability lists, MSI-X, the bus walk, virtio's PCI transport, MSI-X messages and the pages of a BAR a driver must not be given. Has its fuzz target and its Miri step already. | 51 |
| `src/lib/proto/native-abi` | Reached at 9 — native syscall numbers, handles, rights, signals, `errno` names, `repr(C)` layouts. Constants only, like `src/lib/proto/linux-abi`, and tested against it. | 23 |
| `src/lib/kernel/objects` | Reached at 9 — the handle table and the channel message queue, generic over what a handle names; every process's table and every channel is one; and the reachability walk a send makes before it queues an endpoint. Has its fuzz target and its Miri step. | 29 |
| `src/lib/fs/btrfs/format` | 11, 12 — written ahead as superblock, chunk tree, B-tree nodes and item payloads, parsing only; since stage 11 the whole read path, to directories and file contents through a `volume::Device` the caller implements, with its zlib, LZO and zstd decoders. Stage 12's write side is `src/lib/fs/btrfs/write`, and the VFS half `src/lib/fs/btrfs/vfs`, both written in their stage. | 167 |
| `src/lib/network/netwire` | Networking — the headers: Ethernet with one 802.1Q tag, ARP, IPv4 with its options, IPv6 with the extension-header walk, ICMPv4, ICMPv6 and Neighbor Discovery, UDP, and TCP with the options a connection negotiates. Parsed without allocation and emitted into the caller's buffer, with each format's checksum verified where it carries one. Has its fuzz target, which requires every header that parses to emit and parse back unchanged. | 54 |
| `src/lib/network/nettcp` | Networking — the TCP state machine over `src/lib/network/netwire`'s headers: the eleven states of RFC 9293 in the standard's order, including simultaneous open and simultaneous close; reassembly of what arrives out of order; window scaling and the maximum segment size; selective acknowledgment blocks for what is missing; Nagle, delayed acknowledgments, silly-window avoidance and the zero-window probe; retransmission timing by RFC 6298 with Karn's algorithm and Linux's bounds; and NewReno slow start, congestion avoidance, fast retransmit and fast recovery. It holds no clock, no socket and no address, so its tests drive two connections against each other across a wire the test loses and delays segments on, at a clock it advances by hand. Has its fuzz target. | 31 |
| `src/lib/proto/displayctl` | 17 — the control protocol between the kernel's display core and a ring-3 display driver (`docs/DISPLAY.md` §2.2): twelve fixed little-endian messages, thirteen since stage 19's `ATTACH_OBJECT`, sixteen with the cursor's two and `MODES`, decoded strictly, reserved bytes included, and the core's side of the conversation as a state machine of fixed capacity that accepts only the reply it is waiting for — an ATTACHED it asked for, the oldest FLIPPED, a DETACHED it asked for — or a `MODES` that describes the card `HELLO` did, and stays broken once a driver lies. No ring: frames do not move, the card VMO's ranges are the device's backing, or since stage 19 a GPU object `ATTACH_OBJECT` names. Has its fuzz target. | 33 |
| `src/lib/proto/inputctl` | 17 — the control protocol between the kernel's input core and a ring-3 input driver (`docs/INPUT.md` §3.2), and evdev's per-open queues (§3.1): six fixed little-endian messages decoded strictly, a HELLO checked field by field in the order it is read, and the core's side of the conversation, which publishes only the event types the input iteration supports, refuses a whole EVENTS holding one event the driver did not declare or a report over 256 events, keeps the key, LED, switch, axis and repeat state as Linux's `input_get_disposition` does, and hands each whole report to the grabbing open or to every open. The queue is Linux evdev's ring as `drivers/input/evdev.c` has it: `SYN_DROPPED` and the newest event when it fills, nothing readable past the last `SYN_REPORT`, the flush a state read makes and the drop a clock change makes, and `read`'s errors in evdev's order, writing `input_event` at either width in the open's clock. Has its fuzz target. | 26 |
| `src/lib/drivers/display/virtio-gpu` | 17 — the virtio-gpu 2D driver logic over the same shape of traits `src/lib/drivers/block/virtio-blk` uses, so it holds no handle: bring-up with only the control queue, one command at a time with its request at the start of a command area and its response at the end, every response checked; and the pipeline from the display core's ATTACH, SCANOUT, FLUSH and DETACH to the device commands each takes, undoing a failed step as far as is safe and never unpinning pages the device may still hold. Its tests drive a frame through to a fake device's screen; its fuzz target checks one reply per request and no unpin without a pin. | 31 |
| `src/lib/drivers/net/virtio-net` | 10, networking — the virtio-net driver logic over the same traits `src/lib/drivers/block/virtio-blk` uses, so it holds no handle and does no I/O of its own: bring-up in the order the status protocol fixes, both queues sized and activated before `DRIVER_OK`, a receive queue filled at bring-up and refilled as frames are taken — an empty one drops every frame in silence — a transmit queue whose buffers stay the caller's until the device says it has read them, and a drain that acknowledges the interrupt first so a completion landing during it raises another rather than being lost. It negotiates no checksum, segmentation or merge-buffer feature, which is what makes a received frame one buffer and every header the twelve bytes `VIRTIO_F_VERSION_1` makes it. Has its fuzz target. | 22 |
| `src/lib/network/net` | Networking — the net core over the two above: interfaces and their addresses, one routing table for both families with longest-prefix and metric order, a neighbour cache that answers ARP's question and Neighbor Discovery's the same way and holds the packets waiting for either, IPv4 fragmentation and reassembly bounded so a stranger cannot fill this host's memory, ICMP echo both ways including the unprivileged socket `ping` uses and the unreachable a closed port earns, UDP with Linux's socket-matching order, and TCP connections and listeners. A packet routed to the loopback goes back into the input path instead of out of a driver, so a host talks to itself with no device at all. Has its fuzz target. | 72 |
| `src/lib/proto/netring` | Networking — the net ring, `docs/NET-RING.md` in code: the memory the kernel shares with a ring-3 network driver. The block ring's discipline with its allocator removed, because a frame is bounded by the MTU: the data VMO is `entries` slots of a fixed size and a submission names its slot, which takes away the class of bug where a region is reused before its completion — on an untranslated domain, a device writing into somebody else's packet. Private indices, checked reads of the peer's, the want-bell handshake, and every entry checked when it is read; corruption is terminal for the side that sees it. | 32 |
| `src/lib/network/netlink` | Networking — reached already, by the `AF_NETLINK` sockets above: walking a buffer of netlink messages and the attributes after each fixed header, and building replies into a caller's buffer with every length and pad computed rather than taken. The walks refuse a length below the header they introduce, one past the end, and the zero that walks the same message for ever, and every step forward is at least a header wide, so a walk over any bytes ends. Its `netlink_walk` fuzz target requires that, requires what a walk borrows to lie inside the input, and requires anything the builder writes to walk back to what was built. | 48 |
| `src/lib/drivers/net/netserve` | Networking — a ring-3 network driver's serve loop, between the net ring and a virtio-net device. The two directions are not symmetrical and that is the design: sending is a copy and a submission, while a frame arrives into a buffer the *device* chose and takes the oldest receive slot the kernel posted, or is dropped if none is waiting. A submission is never taken that cannot be answered, a device buffer goes back the moment its bytes are copied, and frames the device refuses wait in the order the kernel asked for them — a queue and not a single frame, because the ring's head advances for a whole batch and keeping one would drop the rest. | 12 |
| `src/lib/init/svc` | 15 — the service manager's pure core (`docs/INIT.md` §3): unit files in systemd's syntax, read as `conf-parser.c` reads them, the three layered unit directories as a source the backend fills, drop-ins, masking, aliases, templates and specifiers, and every version-1 kind's keys; then the manager, `Manager::step` and `deadline`: the dependency graph, transactions and operations, the slice tree, each kind's state machine, restart policy, boot and shutdown. `no_std`; it names no system call and holds no handle. Its restart policy is `src/lib/init/restart` since L11, which allocates nothing so that `devmgr` can link it too. Has its fuzz targets (`svc_unit`, `svc_manager`) and its Miri step. | 101 |

With the five crates the boot path was built on — `bootinfo`, `elf` (the
loader's), `frame`, `heap`, `paging` — that was **1101 host unit tests** when
it was first counted, plus the doc-tests and the 41 of `xtask` itself. On
2026-09-23 the same crates have 1512, and `xtask` 242. On 2026-09-24 every
crate under `src/lib/` together has 2178, 92 of them `src/lib/init/svc`'s,
and on 2026-09-30 the 66 crates there have 2603.

**The gap this opens, stated rather than hidden.** The continuous rule below
asks for a fuzz target *and* a Miri run per crate, and `src/tests/fuzz/` has
thirty-seven: `elf_parse`, `frame_alloc`, `ustack_build`, `handle_table`,
`vfs_ops`, `pci_walk`, `btrfs_read`, `block_queue`, `blkring`, `virtio_blk`,
`virtio_net`, `cpio_parse`, `fdt_parse`, `acpi_tables`, `netwire_parse`,
`nettcp_state`, `net_input`, `netlink_walk`, `hyprconf_parse`, `virtio_gpu`,
`virtio_input`, `displayctl`, `renderctl`, `inputctl`,
`virtio_gpu_pipeline`, `virtio_input_driver`, `wayland_wire`,
`linux_abi`, `cgroupfs_write`, `sysfs_names`, `svc_unit`, `svc_manager`,
`svc_proto`, `sndctl`, `virtio_snd`, `argon2_phc` and `auth_proto`.
Every crate
in the table above parses bytes that came from outside the system — a disk,
a firmware table, an archive a stranger built — which is precisely the
population the rule was written for. `virtio` was owed a target and has
several now, since the gpu, input, blk and net targets all drive it.
`linux-abi`'s came last, on 2026-09-23, long after stage 7 had consumed the
crate; none is owed now.

`linux_abi` reads a stranger's bytes as every structure a system call
takes by pointer, at both widths: each socket address that parses must
encode to exactly its stated length and parse back to itself, and a buffer
a byte short must be refused; `msghdr`, `cmsghdr`, `ucred`, `linger`, the
netlink headers, `drm_version`, `input_event` and every `layout!` structure
of the DRM, evdev and virtgpu ioctls must read back from what they write,
and a buffer as long as the structure must always read, so a field placed
past the size the probe printed fails here; the control-message walk must
end within one message per header's worth of bytes, keep every message's
data inside the buffer and stop at its first refusal; and an evdev request
number must decode to the direction, number and size that built it.

`cpio_parse` asserts more than the absence of a panic: that every name and
data slice lies inside the archive exactly where the format puts it, that the
summary agrees with the walk and `find` with the first entry of a name, that a
prefix of an archive never reads a different entry, and that any archive
walked to its trailer, written out again from what the reader reported, reads
back identical. Its seeds include the archive every boot image carries, byte
for byte, and two that GNU cpio wrote.

`netwire_parse` runs every header parser in `src/lib/network/netwire` on the same bytes,
with the transport checksums' addresses taken from the input so the fuzzer can
steer them. Each header that parses must lie inside the input, and must emit
and parse back to exactly the same header and payload — TCP's options in
canonical form, Neighbor Discovery by its message body — while the IPv6
extension walk stays inside the payload and a checksum summed in two pieces
equals the checksum of the whole.

`virtio_net` drives the network driver from a device whose every register,
used entry and header byte the fuzzer chose. Beyond the absence of a panic it
requires that a `written` the device invented, a header shorter than the
negotiated length and a descriptor id the driver never handed out each come
back as a `DeviceError` rather than as a read past the end of a buffer, that
every frame the driver reports as received lies inside the region it was given,
and that every frame the caller was allowed to send is answered exactly once —
by a completion, or by the abandoned list after the reset.

`nettcp_state` drives one connection from a stranger's segments: every field
of every segment, interleaved with writes, reads, closes and a clock the
fuzzer moves. Beyond the absence of a panic it requires that every header the
state machine answers with can be written by `src/lib/network/netwire` and parsed back,
that neither buffer grows past the capacity it was built with however many
out-of-order segments arrive, and that a connection which reached `CLOSED`
stays there and sends nothing more. It has already earned its place: it found
a connection closed during its handshake that kept its retransmission timer,
which fired afterwards and rewound the sequence numbers of a connection that
no longer existed.

`net_input` drives a whole host — an interface, an address, a route and four
sockets — from a stranger's frames, with the clock moved by the fuzzer between
them. Every frame the host answers with is parsed back as Ethernet and as the
IP packet inside it, so a header the stack builds that nothing can read is a
crash rather than a packet on a wire. The reassembly ceiling is asserted after
every frame, and a host that has been sent nothing but rubbish is required to
stop talking rather than to keep producing frames for ever.

`netlink_walk` walks the fuzzer's bytes as a buffer of netlink messages and
each message's payload as attributes, from every fixed-body offset a routing
message uses. Beyond the absence of a panic it requires the walk to end — a
buffer of *n* bytes can hold no more than *n*/16 messages, and a walk that
yields more is walking the same bytes twice, which is the hang the target
exists to catch — that everything a walk borrows lies inside the input, that an
error is the last thing a walk yields, and that a message built from a header,
a body and attributes the fuzzer chose walks back to exactly those.

`fdt_parse` holds the device tree reader to a second walk of the token stream
written from the specification: a tree the reader accepts must be well formed
by that walk, `nodes()` must yield exactly its nodes with the cell counts
their parents declared and exactly the properties after each name, and
`find_node` must return the first node the specification's path matching
selects. Its seeds are `dtc`-compiled trees holding every binding the crate
decodes, and token-built shapes `dtc` will not write: NOPs, a property after a
subnode, nesting at and past the depth limit.

`acpi_tables` reads its input as physical memory and walks it the way the
kernel does, from an RSDP at address zero through the root table to every
table listed, and it also reads every table whose signature appears anywhere
in the input. Each table must be exactly its declared length inside that
memory; each MADT entry, MCFG allocation, DMAR structure and IORT node must
decode to the little-endian bytes at the specification's offsets, and be
called malformed exactly when it is too short for its type; `check_entries`
must accept a MADT exactly when its entries tile it; and `Acpi::find` must
return the first listed table of a signature. Its seeds are hand-built with
QEMU's values: a q35 machine with intel-iommu, an AArch64 `virt` machine with
an SMMUv3, an ACPI 1.0 machine, each table alone, and the faults firmware
ships.

`ustack_build` is what the rule looks like when it is followed rather than
recorded as debt: written before a line of stage 7 kernel code existed, and it
found a real gap within a minute. A string with a NUL byte inside it built a
perfectly well-formed image that read back as a *different, shorter* string,
because everything on that stack is recovered by scanning for a NUL. The
builder now refuses it. Nothing about that bug is visible from the kernel side
— it is a program receiving an argument nobody passed it — and it would have
been found, if at all, by whoever was debugging a shell that mangled its own
arguments.

Miri was further behind than fuzzing, and the crates the CI file's own comment
names as the reason the job exists had no step. They have one now: CI
interprets `src/lib/platform/elf`, `src/lib/proto/bootinfo`, `src/lib/kernel/ustack`, `src/lib/kernel/objects`,
`src/lib/fs/common/vfs`, `src/lib/platform/pci`, `src/lib/fs/common/block`, `src/lib/proto/blkring`, `src/lib/proto/native`,
`src/lib/drivers/block/virtio-blk`, and the three the kernel runs on every
allocation and every mapping, `src/lib/kernel/frame`, `src/lib/kernel/heap` and `src/lib/kernel/paging`. None
of the three had undefined behaviour to report. `src/lib/kernel/fallible`,
`src/lib/init/svc` and `src/lib/crypto/argon2` have joined them since, and
CI runs the list in parallel parts. Their local run times were 541
seconds for `frame`, 102 for `heap` and 142 for `paging`; the frame
allocator's long random workload was 97% of the first, and runs 2,000 of its
20,000 steps under Miri. `cargo xtask check --miri` runs the same list, and a
test fails when it and the workflow disagree.

**`src/user/system/linux/ferrousli/` is on the goal's path.** It is a C library for Linux written in
Rust, modelled on musl and aimed in time at glibc's binary interface. It is its
own cargo workspace, depends on no Ferrix crate, and reaches the kernel only
through Linux system calls. Until 2026-09-13 it sat beside this roadmap; by the
customer's decision that day it is on it. `cargo xtask check --ferrousli` runs
its formatting, lints and tests in both profiles, and `docs/BACKLOG.md` says
which landings must pass it. Busybox 1.37.0 built against it by `cargo xtask
busybox` links with no symbol undefined, and since 5e9b0b6 passes `test-shell`
and `test-vfs` on x86_64 without reaching a stub. It is the primary busybox,
the userland Ferrix is measured with: the gates run it first, with `--init
ferrousli`, and keep Alpine's musl build and the glibc one as compatibility
checks. Its test programs already boot as Ferrix's first process with `cargo
xtask test-shell --init`, `src/user/system/linux/ferrousli/tests/c/thread/on_ferrix.c` among them for
`CLONE_THREAD`. Its own status is in [ferrousli's README](https://github.com/ferrix-os/ferrousli/blob/main/README.md),
and its distance from POSIX.1-2024, interface by interface, in
[POSIX-2024.md](../POSIX-2024.md).

---

