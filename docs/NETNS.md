# Network namespaces

Stage 13's network namespace (`docs/roadmap/stage-13-namespaces-cgroups-v2-seccomp.md`),
built on the user namespaces of `docs/NAMESPACES.md` §2.2 (branch
`stage13-n4-userns`): `CLONE_NEWNET` through `clone`, `clone3` and `unshare`,
a namespace of its own interfaces, addresses, routes, neighbours, ports,
sockets, `/proc/net` and netlink view, and `veth` pairs to join two of them.

Status: **being built** (§11). This document is written from the code as it
stands on `stage13-n4-userns` (813d24d4), before any of it is changed.

---

## 1. What is per-namespace today, and what is global (measured)

The stack is already one value. `src/lib/network/net`'s `Stack` holds
**everything** a network namespace owns: the interfaces and their addresses
(`interfaces`), the routing table, the neighbour cache, every socket
(`sockets`, `packets`), the random source ports and sequence numbers come
from, the egress queue and the fragment reassembler. It has no clock, lock or
device. Two `Stack` values are therefore two complete, isolated hosts with no
change to the library's shape; a network namespace is a `Stack` plus what the
kernel adds around one.

What the kernel adds is `net::NetCore` (`src/kernel/src/net/mod.rs`), and it is
a **single global**:

| Global | Where | What |
|---|---|---|
| `CORE: Once<NetCore>`, reached by `net::core()` | `net/mod.rs` | the `Stack` behind one `SpinLock`, `pending` (frames waiting for a driver), `progress` (the one wait queue every socket sleeps on), `transmit_wakers` (interface index to the port of the ring serving it) |
| `FILES: SpinLock<BTreeMap<SocketId, FileOf>>` | `net/socket.rs` | socket id to sockfs inode and owner, for `/proc/net/tcp` |
| `NEXT_PORT` | `net/netlink/mod.rs` | netlink port numbers; only ever compared for difference |
| `PLACED: Vec<(index, device node)>` | `interfaces/net_ring/mod.rs` | which device node serves which interface index |
| `ABSTRACT: BTreeMap<name, Weak<Socket>>` | `fs/sockname.rs` | abstract `AF_UNIX` names; one table for the whole kernel |

Everything else that touches the network reaches the stack through
`net::core()`. The sites (counted 2026-09-30, `grep -c 'core()'`):

* `net/socket.rs` 28 (every `InetSocket` method), `net/packet.rs` 11,
  `net/ifreq.rs` 9, `net/netlink/mod.rs` 5: a socket or an ioctl acts on
  "the" stack. **These become "the socket's namespace"**, because a socket
  holds its namespace.
* `fs/procfs/render.rs` 4 (`/proc/net/{dev,route,tcp*,udp*,arp}`), `fs/sysfs.rs`
  1 (`/sys/class/net`): **these become "the reader's namespace".**
* `interfaces/net_ring/mod.rs` 7 and its check 6: the ring-3 NIC driver
  attaches here. **These stay the first namespace's**, with one indirection
  (§3.2) for a NIC that has been moved.
* `net/check.rs`, the boot check of the stack: the first namespace, unchanged.

Not a site of its own but one that matters: **the driving task** (`net::run`)
ticks `core()` and sleeps to its deadline. With several stacks it must tick
all of them (§2.4).

Loopback is *inside* the stack (`Interface::loopback`, `Medium::Loopback`):
a packet routed to it re-enters the input path on the next `poll_transmit`.
So a new namespace's loopback needs no device, no driver and nothing in the
kernel; it is the library's.

### 1.1 Sockets find their stack through their inode

An `InetSocket` (`net/socket.rs`) is an `Inode` holding a `SocketId` and
calling `net::core()` on every operation. It has no handle on a stack.
`NetlinkSocket` and `PacketSocket` are the same. A unix socket
(`fs/socket.rs`) never touches the stack, but binds abstract names in the
global `ABSTRACT` table.

### 1.2 How the ring-3 NIC driver attaches

A driver (virtio-net, in `src/user/drivers`) offers a ring; the kernel's
`interfaces::net_ring` accepts the HELLO, **adds an `Interface` to `core()`**
(`add_or_take_up`, which returns its index), registers a `transmit_waker`
for that index, and serves the ring: frames the stack queued for the index are
taken with `core().take_outgoing(index, room)`, frames the driver received go
in with `core().receive(index, frame)`, link changes with `set_carrier`. A
driver that dies leaves its interface *parked* (`park_interface`): index,
addresses and routes kept, carrier down, for the next ring made for the same
device. `PLACED` remembers which device node an index came from.

---

## 2. The model

### 2.1 A namespace

```
NetNamespace
  id: u64                       what /proc/<pid>/ns/net names (net:[id])
  owner: Arc<UserNamespace>     the user namespace that was current when it was made
  core: NetCore                 the Stack, pending, progress, transmit_wakers: as today
  tables: Option<SpinLock<Charge>>   F-37; None for the first
  veth_out: per-call, see §3.3
  _charge: Option<Charge>       F-37; None for the first
```

`net::core()` remains, and answers **the first namespace's** `NetCore`, so the
driver, the boot checks and every kernel-side caller keep their signature.
`net::first()` is the `Arc<NetNamespace>` itself, made on first use. The first
namespace has Linux's own number, `NET_NS_INIT_INO` (0xF0000098).

A process holds its namespace: `Process::net_ns: SpinLock<Arc<NetNamespace>>`,
a fork child copies it, and every process that nothing forked starts in the
first. (Linux keeps it per task in `nsproxy`; `stage13-smallns` keeps the UTS,
IPC and cgroup namespaces in a per-process `NsProxy`, as Ferrix has no
per-thread credentials either. When the integrator merges, this field moves
into the `NsProxy`; the accessor `Process::net_ns()` is the seam.)

A socket holds its namespace for life: `InetSocket`, `NetlinkSocket`,
`PacketSocket` and the unix `Socket` each carry an `Arc<NetNamespace>` (the
unix one only its id, for the abstract table). A process that leaves
(`unshare(CLONE_NEWNET)`) keeps every socket it made, and they go on working in
the namespace they were made in, as Linux's `sk->sk_net` does.

### 2.2 Making one

`CLONE_NEWNET` (0x40000000) via `clone`, `clone3` and `unshare`, alone or with
`CLONE_NEWUSER` (then the new user namespace owns it). Refused as Linux does:

* `EPERM` without `CAP_SYS_ADMIN` in the user namespace the creator will be
  in (the new one if `CLONE_NEWUSER` is asked with it);
* `ENOMEM` past the job's memory (§5);
* `EINVAL` with `CLONE_THREAD` (a thread cannot leave its group's network
  namespace here; **a deviation**: Linux allows `unshare(CLONE_NEWNET)` in one
  thread of several, because its namespaces are per task; ours are per
  process, as the mount and user namespaces already are, and `unshare` with
  more than one thread is `EINVAL`).

A new namespace has **one interface, `lo`, down, with no address and no
route** (Linux's state). `ip link set lo up` (`RTM_NEWLINK` or `RTM_SETLINK`
with `IFF_UP`, or `SIOCSIFFLAGS`) brings `127.0.0.1/8` and `::1/128` with
their routes. Taking `lo` down removes its addresses and routes, as Linux does.
The first namespace's `lo` is up from boot, as today. Nothing else is in it:
no route, no default gateway, no NIC.

### 2.3 Permissions

* **Creating:** `CAP_SYS_ADMIN` in the creator's user namespace (above).
* **Changing a namespace** (every netlink request that changes something, every
  `SIOCS*`): `CAP_NET_ADMIN` **over the owning user namespace**
  (`capable_over(creds, &ns.owner, CAP_NET_ADMIN)`), which for a process in
  the first user namespace is root, for the creator of a child user namespace
  is the owner rule of `cap_capable`, and for a process inside the user
  namespace that owns it is its capability bit. A process in a child user
  namespace has *no* power over a network namespace the first user namespace
  owns (rule NN6).
* **Raw and packet sockets:** `CAP_NET_RAW` over the owning user namespace of
  the socket's namespace.
* **Reading** (dumps, `SIOCG*`, `/proc/net`): unprivileged, as today.
* **Moving an interface** into another namespace, or creating a veth end there
  (`IFLA_NET_NS_PID`, `IFLA_NET_NS_FD`): `CAP_NET_ADMIN` over the owner of
  the **source and the target**, so an unprivileged user namespace cannot push
  a veth end into someone else's namespace (rule NN9).

`userns::HONOURED` gains `CAP_NET_ADMIN` (12) and `CAP_NET_RAW` (13) (U8 of
`docs/NAMESPACES.md` §4 says every honoured capability is deliberate; these
two are: a network namespace's owner must be able to configure its own
devices, and only them; neither reaches a file, a process or the first
namespace). Each gets a boot check and a negative control (NN6, NN7).
`CAP_NET_BIND_SERVICE` (10) is **not** honoured: Ferrix has no privileged-port
rule at all today (a port below 1024 binds for anyone), so there is nothing
to relax; it is recorded, not granted.

### 2.4 Who ticks the stacks

`net::run` (the driving task) loops over `live namespaces`, a list of
`Weak<NetNamespace>` kept in a leaf lock (`NAMESPACES`), ticking each and
taking the earliest deadline; a dead `Weak` is dropped. Cost with only the
first namespace: one list element, one `upgrade`. The 50 ms recheck bounds
a missed wake-up as it does today.

### 2.5 A namespace ends

with the last `Arc`: the processes in it, the sockets made in it and any
`nsfs`-style file naming it. `Drop for NetNamespace` (nothing else runs
there, the stack is exclusively owned):

* a **physical** interface (`Backing::Device`) goes back to the first
  namespace with a fresh index there, its carrier and flags kept, its addresses
  and routes dropped (as Linux's `default_device_exit` does, which moves the
  device to `init_net`);
* a **veth** end is destroyed, and so is its peer wherever it is (Linux's
  `veth_dellink` deletes the pair);
* sockets are closed with the stack, TCP connections reset silently (a
  namespace that is gone sends nothing).

---

## 3. Interfaces

An `Interface` gains `backing: Backing` (library, `iface.rs`):

```
enum Backing { Software, Device(u32), Veth { pair: u64, end: u8 } }
```

`Software` is `lo` and anything the stack makes itself. `Device(key)` is a
NIC a ring-3 driver serves; `key` is stable across moves and reboots of the
driver (§3.2). `Veth` names the pair and which of its two ends it is.

### 3.1 Library changes (`src/lib/network/net`, host-tested)

* `Stack::new_namespace(config)`: a stack whose `lo` is down and has no
  address. `Stack::new` stays as it is for the first namespace and for every
  existing test.
* `Stack::set_up(lo, true)` adds the loopback addresses and routes if absent;
  `set_up(lo, false)` removes the addresses, routes and neighbours.
  For any other interface `set_up` is unchanged, except that a `Veth` end is
  not `IFF_RUNNING` until its peer is up too (the kernel sets the carrier
  flags, §3.3).
* `Stack::detach_interface(index) -> Option<Interface>` and
  `Stack::attach_interface(Interface) -> Result<u32, Error>` (the name must be
  free there; a fresh index; addresses kept, routes and neighbours not): the
  two halves of moving an interface.
* `Stack::rename_interface`.
* `Reassembler::with_limit`: a non-first namespace reassembles at most
  `NAMESPACE_REASSEMBLY` (32 KiB) rather than 256 KiB, and the kernel charges
  what is held (§5).
* `Stack::interface_count`, route and address counts (for the budget of §5).

### 3.2 Physical NICs and the driver

The ring serves an interface by **device key**, not by index. A new
`net/device.rs` holds `DEVICES: SpinLock<Vec<Device>>` with
`Device { key, namespace: Weak<NetNamespace>, index, node }`, a leaf lock.
`add_or_take_up` (which still adds to the first namespace) registers a key and
puts `Backing::Device(key)` on the interface; `take_outgoing`, `receive`,
`set_carrier`, `park_interface`, `forget_interface` and `wake_on_transmit`
become `device::…(key, …)` and look the namespace and index up, then do what
they did. **With the NIC in the first namespace the cost is one lookup in a
vector of one or two elements per call.** `PLACED` is keyed by key.

Moving a `Device` interface (`IFLA_NET_NS_*` on it, or the end of a namespace
holding it) is `detach` from one stack, `attach` to the other, the
`transmit_waker` moved with it, the registry updated; frames in flight for the
old index are dropped. A parked interface (driver gone) moves like any other
and keeps its key, so the next ring for that device takes it up wherever it
is.

### 3.3 veth

A pair is a kernel record `VethPair { id, ends: SpinLock<[End; 2]> }` with
`End { namespace: Weak<NetNamespace>, index: u32 }`, kept in a leaf-locked
table `VETHS: BTreeMap<u64, Arc<VethPair>>`. Both interfaces are Ethernet,
MTU 1500, `IFF_BROADCAST | IFF_MULTICAST`, a random locally administered
MAC (`02:…`), names `veth0`, `veth1`… (first free in the namespace it is made
in; `IFLA_IFNAME` overrides).

**Transmit.** `NetCore::take_frames` (called with the stack locked) already
walks `poll_transmit`. A frame for an interface whose backing is `Veth` is
pushed on a small `Vec<(pair, end, frame)>` instead of `pending`. `NetCore::with`
returns it to the caller **after the lock is dropped**, and the caller runs
`net::forward`:

```
forward(work): while let Some((pair, end, frame)) = work.pop():
    (target, index) = VETHS[pair].ends[1 - end]      // leaf locks, released
    if target's interface is up:
        work.extend( target.core.receive_collect(index, frame) )
```

`receive_collect` is `receive` that returns, rather than forwards, the veth
frames the reception produced (an ARP reply, a SYN-ACK). So the whole exchange
is an **iterative loop over an explicit work list**: no recursion, no two
stack locks ever held together, bounded by `MAX_FORWARD` (4096 frames per
top-level call; the rest are dropped, as a network drops). The sender's task
runs the receiver's stack, exactly as a loopback packet runs in the sender's
task today.

**Carrier.** After any change to an end's up flag the kernel calls
`veth::refresh(pair)`: each end gets `IFF_RUNNING | IFF_LOWER_UP` iff both are
up. A frame is delivered only to an end that is up.

**Moves and deletion.** `RTM_DELLINK` of an end deletes both (`EOPNOTSUPP`
for `lo` and a NIC, as Linux). A move is `detach`/`attach` with the pair
record's `End` updated between the two. A namespace ending destroys its ends
(§2.5). The pair record is removed with the second end.

### 3.4 Interface indexes and names

Indexes are per namespace (`max + 1` of that stack's interfaces, as now).
Linux keeps an interface's index across a move when it is free in the target;
this does not (a move gives the next free index). A program that cached the
index of a moved interface looks it up again, which `ip` does. **A deviation**,
recorded. Names must be unique in the target (`EEXIST`; Linux renames to
`dev%d`, we refuse).

---

## 4. The places that change

| Site | Change |
|---|---|
| `net/mod.rs` | `NetNamespace`, `first()`, `create()`, `acting()`; `core()` = first's; `NetCore` made per namespace; `take_frames`/`with` hand veth frames to `forward`; `run` ticks every namespace |
| `net/namespace.rs` (new) | the object, its charge, `Drop`, the registry of live namespaces, `forward`, `veth`, budget |
| `net/device.rs` (new) | device keys and the registry of §3.2 |
| `net/socket.rs` | `ns: Arc<NetNamespace>` in `InetSocket`; `FILES` keyed `(ns id, SocketId)`; `open` takes the namespace; `accept`'s wrap uses the listener's |
| `net/packet.rs`, `net/netlink/mod.rs` | `ns` field; `CAP_NET_ADMIN` test replaces `privileged()` |
| `net/netlink/route.rs` | rename; `IFF_UP` of `lo` goes through `set_up`; budget hooks |
| `net/netlink/link.rs` (new) | the requests that cannot run under the stack lock because they touch two stacks or allocate: link create (veth), `RTM_DELLINK`, `IFLA_NET_NS_*` |
| `net/ifreq.rs` | the namespace of the calling process; `CAP_NET_ADMIN` over its owner |
| `syscall/sockets.rs` | `socket()` opens in the process's namespace; raw/packet need `CAP_NET_RAW` over its owner |
| `syscall/userns.rs` | `HONOURED` += `CAP_NET_ADMIN`, `CAP_NET_RAW` |
| `syscall/process.rs` | the `net_ns` field, copied in `forked_into` |
| `syscall/family.rs`, `syscall/namespace.rs` | `CLONE_NEWNET` in `namespaces_asked`, `clone_with`, `sys_unshare` |
| `fs/sockname.rs`, `fs/socket.rs` | abstract names keyed `(net ns id, name)` |
| `fs/procfs.rs`, `fs/procfs/render.rs` | `/proc/<pid>/ns/net`; `/proc/net/*` from the reader's namespace |
| `fs/sysfs.rs` | `/sys/class/net` from the reader's namespace |
| `interfaces/net_ring/mod.rs` | device keys (§3.2) |
| `fs/kmem_check.rs` | fills: network namespaces, veth pairs |
| `net/netns_check.rs` (new), `stages_check.rs`, `panic/catalog.rs` | the `netns` line, FX-0893 |

`/proc/<pid>/net` (the per-process view) is not built: `/proc/net` shows the
reader's namespace (Linux's `/proc/net` is `self/net`), and a program that
wants another process's goes through `setns` and then reads it. **Not built.**

---

## 5. F-37: every kernel object a program can make and keep is charged

| Kind | Made by | Charged |
|---|---|---|
| network namespace: itself, its `lo`, its routing and neighbour headroom | `CLONE_NEWNET` | at creation, `arc_footprint::<NetNamespace>() + NAMESPACE_BASE` |
| interface, address, route | netlink, `ioctl` | the **tables charge**: after a change the namespace resizes a `Charge` (to the job that made the namespace, as `Charge::grow` charges the same job) to `ENTRY_COST × (interfaces + addresses + routes)` and to the reassembler's held bytes; a change that would pass the limit is refused `ENOMEM` *before* it is applied, the charge taken with headroom for the one entry the message adds |
| veth pair: two interfaces, the pair record, its table node | `RTM_NEWLINK` | at creation, to the job of the caller, the pair holding its own `Charge` (both ends' worth), released when the second end goes |
| neighbour cache | ARP | bounded at 256 entries by the library (`MAX_ENTRIES`), inside `NAMESPACE_BASE` |
| sockets, connections, queued datagrams | as today | as today (`socket_charge`, the stack's `owner`) |
| netlink socket | `socket()` | as today |

An unprivileged user can now reach the netlink writers (by owning a network
namespace), which before they could not; the tables charge is what makes that
safe to allow, and the ceilings of §7 bound what a single namespace holds.
`kmem_check` gains two fills (namespaces made by `unshare(CLONE_NEWNET)` in a
job until `ENOMEM`; veth pairs made in one namespace until `ENOMEM`), each
refused by the limit, a sibling job then making one, and both jobs reading
zero after.

---

## 6. Locks

| Lock | Kind | Guards | Order |
|---|---|---|---|
| `NetCore::stack` | `SpinLock` | one namespace's `Stack` | after nothing; **never two at once** (the forward loop takes them one after the other) |
| `NetCore::pending`, `transmit_wakers` | `SpinLock` | as today | after `stack`, as today |
| `NAMESPACES` | `SpinLock` | the list of live namespaces | a leaf; cloned out before any tick |
| `VETHS`, `VethPair::ends` | `SpinLock` | the pair table, each pair's two ends | leaves; never held with a stack lock; released before a stack is entered |
| `DEVICES` | `SpinLock` | the device registry | a leaf, as above |
| `Process::net_ns` | `SpinLock` | the pointer | a leaf; cloned out |
| `tables` | `SpinLock` | the charge | a leaf; taken *after* a `look` of the counts has released the stack |
| `FILES` | `SpinLock` | socket files by `(ns, id)` | never with a stack lock, as today |

A change that touches two namespaces (a move) is: lock A, detach, unlock; lock
B, attach, unlock; update the registries in between. The interface is in
neither for that interval and frames for it are dropped. Everything that can
allocate or be refused (the charge, the `Interface`'s addresses) happens
before a stack lock is taken.

`Drop for NetNamespace` runs with no lock held by its caller (the last `Arc` is
dropped after an operation's locks are released; `Process`'s field is dropped
with the process, `InetSocket`'s with the file).

---

## 7. Limits

* Interfaces per namespace: 64; addresses per namespace: 256; routes per
  namespace: 1024 (`ENOSPC`). Each bounds what one namespace's tables can hold
  before its charge is asked, and `ENTRY_COST × 1344` is the most a namespace's
  tables charge can reach.
* veth pairs are charged; there is no separate count.
* Namespaces: no count of their own; the job's memory (§5), as for user
  namespaces.
* forward loop: 4096 frames per call (§3.3).
* Reassembly: 32 KiB in a non-first namespace.

---

## 8. Security: a network namespace grants nothing outside itself

Each rule has a boot check that attempts it and a negative control (`§9`).

* **NN1. A new namespace has a down `lo` and nothing else.** No address, no
  route, no NIC: `bind` to 127.0.0.1 is `EADDRNOTAVAIL`, `connect` to it
  `ENETUNREACH`.
* **NN2. `lo` up brings 127.0.0.1 and `::1`.** UDP and TCP between two sockets
  of the namespace then work; `lo` down removes them again.
* **NN3. Namespaces do not share ports or sockets.** The same port binds in
  both; a listener in one is not reachable from the other over `127.0.0.1`;
  `/proc/net/tcp` lists only the reader's.
* **NN4. A socket belongs to the namespace it was made in, for life.** One made
  before `unshare` still reaches its old namespace's listener after; one made
  after does not.
* **NN5. Creating needs `CAP_SYS_ADMIN` in the creator's user namespace.**
  Uid 1000 is `EPERM`; with `CLONE_NEWUSER` it is allowed.
* **NN6. Changing needs `CAP_NET_ADMIN` over the owning user namespace.** Uid
  1000 cannot bring `lo` up in the first namespace; fake root in a user
  namespace cannot either (it holds the capability only over what its
  namespace owns); the same fake root can in a network namespace its user
  namespace made. (`HONOURED`'s new bit has its control.)
* **NN7. Raw and packet sockets need `CAP_NET_RAW` over the owner.**
* **NN8. A veth pair joins two namespaces.** Created by `RTM_NEWLINK`
  (`IFLA_LINKINFO` kind `veth`, peer in `IFLA_INFO_DATA`), one end moved by
  `IFLA_NET_NS_PID`, addressed and routed, a UDP datagram and a TCP stream
  cross it; a third namespace sees nothing (NN16).
* **NN9. Moving needs `CAP_NET_ADMIN` over source and target.** An unprivileged
  user namespace cannot push an end into the first namespace or another
  user's.
* **NN10. A physical interface stays in the first namespace unless a
  privileged caller moves it, and returns when the namespace ends.**
* **NN11. Abstract `AF_UNIX` names are per network namespace.**
* **NN12. `/proc/<pid>/ns/net` names the namespace; `/proc/net` is the
  reader's.**
* **NN13. A namespace ends with its last user.** Its veth ends vanish, peer
  included; its physical interfaces go home.
* **NN14. Charged (F-37):** namespaces and veth pairs, to the job, refused
  `ENOMEM` at its limit.
* **NN15. Flags:** `CLONE_NEWNET` with `CLONE_THREAD` is `EINVAL`;
  `unshare` from a process with other threads is `EINVAL`.
* **NN16. A frame leaves by one veth end and arrives at its peer only.**
* **NN17. A namespace's tables are bounded:** the 65th interface, 257th address
  and 1025th route are refused, and a change past the charge is `ENOMEM`.

**What stays as it is:** `setns(CLONE_NEWNET)` (the small namespaces' branch
owns `setns` and `nsfs`; `net/netns_file.rs` here supplies what a `setns` and
`IFLA_NET_NS_FD` need from a network namespace and the integrator wires it);
pid, IPC, UTS and cgroup namespaces; `AF_UNIX` path sockets (the mount
namespace's business); `SO_PEERCRED` (the user namespace's).

---

## 9. Checks

`net/netns_check.rs`, the `netns` line (FX-0893), registered beside
`check_user_namespaces` in `stages_check.rs`. It makes a process that is uid
1000, drives `clone`/`unshare`/`socket`/`sendmsg` through the syscall layer
where a rule is about a call (NN5, NN6, NN7, NN9, NN15), and drives
`InetSocket` in the two namespaces directly where it is about data (NN1 to
NN4, NN8, NN16). One check per rule above, and one **negative control** per
rule: a one-line sabotage, run, never committed, after which the boot stops
with the check's own message. The controls and their messages are in the commit
that lands the check. The fills of NN14 are in `fs/kmem_check.rs` and print on
the `kmem` line. Host tests in `src/lib/network/net` cover the library half:
`lo` down/up, detach/attach, two stacks joined by a veth, reassembly's limit.

---

## 10. Risks

* **Forwarding runs the peer's stack in the sender's task.** Nothing sleeps in
  it (the stack lock is a `SpinLock`), but a long exchange is long; the bound
  on frames per call keeps it finite.
* **`Drop` at an awkward moment.** A namespace can end in the net task's own
  iteration (it holds a temporary `Arc`). `Drop` takes only leaf locks and the
  dead stack's, and never the net task's.
* **Merge.** Another branch moves per-process namespace pointers into an
  `NsProxy` and `nsfs` and changes `family.rs`'s `namespaces_asked`, the same
  lines this touches. The seams are `Process::net_ns()`, one `CLONE_NEWNET`
  arm in `namespaces_asked`, and `netns_file::{of, location}`.

---

## 11. Where it stands (2026-09-30)

Written before the code. See below as it is built.
