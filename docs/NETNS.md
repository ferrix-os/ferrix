# Network namespaces

Stage 13's network namespace (`docs/roadmap/stage-13-namespaces-cgroups-v2-seccomp.md`),
built on the user namespaces of `docs/NAMESPACES.md` §2.2 (branch
`stage13-n4-userns`): `CLONE_NEWNET` through `clone`, `clone3` and `unshare`,
a namespace of its own interfaces, addresses, routes, neighbours, ports,
sockets, `/proc/net` and netlink view, and `veth` pairs to join two of them.

Status: **built on `stage13-netns`, not landed** (§11). Sections 1 to 10 were
written from the code as it stood on `stage13-n4-userns` (813d24d4) before
any of it was changed, and amended where the building found them wrong; §11
says what differs.

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
  _charge: Option<Charge>       F-37; None for the first
```

`net::core()` remains, and answers **the first namespace's** `NetCore`, so the
driver, the boot checks and every kernel-side caller keep their signature.
`net::first()` is the `Arc<NetNamespace>` itself, made on first use. The first
namespace has the number a Linux host usually shows for its own
(`net:[4026531992]`, 0xF0000098); the others count up from a counter of their
own, so no two kinds of namespace share a number.

A process holds its namespace: `Process::net_ns: SpinLock<Option<Arc<NetNamespace>>>`,
`None` standing for the first (which is not built until something asks, so
making a process does not start the net core); a fork child copies it, a
native child made by `process_create` takes its creator's, and every process
that nothing forked starts in the first. (Linux keeps it per task in `nsproxy`; `stage13-smallns` keeps the UTS,
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
  address or route, and whose reassembler holds at most 32 KiB
  (`reassembly::NAMESPACE_BYTES`). `Stack::new` stays as it is for the first
  namespace and for every existing test.
* `Stack::set_up(lo, true)` gives a loopback its addresses and their routes if
  it has none; `set_up(lo, false)` takes the addresses, routes and neighbours
  away. For any other interface `set_up` is unchanged, except that a `Veth`
  end is not `IFF_RUNNING` until its peer is up too (the kernel sets the
  carrier flags, §3.3).
* `Stack::detach_interface(index) -> Option<Interface>` and
  `Stack::attach_interface(Interface) -> Result<u32, Error>` (the name must be
  free there; a fresh index; routes and neighbours are not restored): the two
  halves of moving an interface. `remove_interface` is `detach` and a drop.
* `Stack::rename_interface`, `Stack::address_count`.
* `Stack::footprint` and `Stack::shed`: the heap the tables hold (interfaces
  and their addresses, routes, the neighbour cache with the packets it holds
  back, fragments), which §5 charges, and what a namespace gives up when it
  cannot pay (what was learned: neighbours and fragments; never what was
  configured).
* `Reassembler::with_limit`, `flush`; `Neighbors::footprint`, `shed`.

Ten host tests (`tests/namespaces.rs`): a new namespace's loopback; a refused
bind and send while it is down; datagrams once it is up; down taking the
addresses away; two namespaces sharing no port; an interface moving with its
addresses and leaving its routes; names; two stacks talking over a pair while a
third hears nothing; the reassembly ceiling; the footprint and the shed.

### 3.2 Physical NICs and the driver

The ring serves a device by a **key**, not by the interface's index. A new
`net/device.rs` holds a leaf-locked registry `Vec<Device { key, namespace:
Weak<NetNamespace>, index }>`. `add_or_take_up` (which still adds to the first
namespace) makes a key, puts `Backing::Device(key)` on the interface, and
places it; `take_outgoing`, `receive`, `set_carrier`, `park`, `remove` and
`wake_on_transmit` are `device::...(key, ...)`, which look the namespace and
index up and do what `net::core()` did. The ring's `Serving` holds the key.
**With the NIC in the first namespace the cost of a call is one look in a
vector of one or two elements.** `PLACED` is keyed by key, and `node_of(index)`
finds the key through the reader's namespace, so sysfs shows a moved NIC in the
namespace it moved to.

Moving a `Device` interface (`IFLA_NET_NS_*` on it, or the end of the
namespace holding it) is `detach` from one stack, `attach` to the other, the
`transmit_waker` moved with it, the registry updated (`namespace::transfer`);
frames in flight for the old index are dropped. A parked interface (driver
gone) moves like any other and keeps its key, so the next ring for that device
takes it up wherever it is. It comes down and loses its addresses on the way,
as Linux's `dev_change_net_namespace` does.

### 3.3 veth

A pair is a kernel record `Pair { ends: SpinLock<[End; 2]>, _charge }` with
`End { namespace: Weak<NetNamespace>, index: u32 }`, kept in a leaf-locked
table `PAIRS: BTreeMap<u64, Arc<Pair>>` (`net/veth.rs`). Both interfaces are
Ethernet, MTU 1500, `IFF_BROADCAST | IFF_MULTICAST`, a locally administered
MAC (`02:fe:...`), named `IFLA_IFNAME` or else the first free `vethN` in the
namespace.

**Transmit.** `NetCore::take_frames` (called with the stack locked) already
walks `poll_transmit`. A frame for an interface whose backing is `Veth` goes
into a short list of `Frame { pair, end, bytes }` instead of `pending`.
`NetCore::with` runs `veth::forward` on it **after the lock is dropped**:

```
forward(frames): work = queue of frames
  while frame = work.pop_front():
    (target, index) = peer(frame.pair, frame.end)     // leaf locks, released
    work.extend( target.core().receive_crossing(index, frame.bytes) )
```

`receive_crossing` hands the frame to the stack if that end is up and returns,
rather than carries, the frames the reception produced (an ARP reply, a
SYN-ACK). So the whole exchange is an **iterative loop over a queue**: no
recursion, no two stack locks ever held together, bounded by `MAX_FORWARD`
(4096 frames per call; the rest are dropped, as a network drops). The sender's
task runs the receiver's stack, as a loopback packet runs in the sender's task
today.

**Carrier.** After a changing request the namespace calls `refresh_carriers`,
which for each pair with an end there sets `IFF_RUNNING | IFF_LOWER_UP` on both
ends if both are up and clears them if not.

**Moves and deletion.** `RTM_DELLINK` of an end deletes both (`EOPNOTSUPP` for
`lo` and a NIC, as Linux). A move updates the pair record's `End` after the
`attach` and refreshes the carrier. A namespace ending destroys its ends (§2.5).
A link dump says `veth` in `IFLA_LINKINFO` for an end.

### 3.4 Interface indexes and names

Indexes are per namespace (`max + 1` of that stack's interfaces, as now).
Linux keeps an interface's index across a move when it is free in the target;
this does not (a move gives the next free index). A program that cached the
index of a moved interface looks it up again, which `ip` does. **A deviation**,
recorded. Names must be unique in the target (`EEXIST`; Linux renames to
`dev%d`, we refuse).

---

## 4. The places that changed

| Site | Change |
|---|---|
| `net/mod.rs` | `core()` = the first namespace's; `NetCore` made per namespace, `with` hands veth frames to `forward`; `run` ticks every live namespace |
| `net/namespace.rs` (new) | `NetNamespace`, `first()`, `create()`, `acting()`, the list of live namespaces, `fit`/`admit` (the tables charge), `transfer` (a move), `Drop` (an end) |
| `net/veth.rs` (new) | pairs: `create`, `destroy`, `peer`, `forward`, `refresh`, `moved` |
| `net/device.rs` (new) | device keys and the registry of §3.2; `come_home` |
| `net/netns_file.rs` (new) | a network namespace as a file, for `IFLA_NET_NS_FD`; the smallest `nsfs` (§11, merge) |
| `net/socket.rs`, `net/packet.rs`, `net/netlink/mod.rs` | `ns: Arc<NetNamespace>`; `FILES` keyed `(ns id, SocketId)`; `open` takes the namespace; `CAP_NET_ADMIN` over the owner replaces `privileged()` |
| `net/netlink/route.rs` | `answer` dispatches per message, asks the charge before an add, settles after a change, refuses past the ceilings; rename; `veth` in a dump |
| `net/netlink/link.rs` (new) | what cannot run under one stack's lock: veth create, `RTM_DELLINK`, `IFLA_NET_NS_PID`/`_FD` |
| `net/ifreq.rs` | the calling process's namespace; `CAP_NET_ADMIN` over its owner |
| `syscall/sockets.rs` | `socket()` in the process's namespace; raw/packet need `CAP_NET_RAW` over its owner |
| `syscall/userns.rs` | `HONOURED` += `CAP_NET_ADMIN`, `CAP_NET_RAW` |
| `syscall/process.rs` | `net_ns`, copied in `forked_into`; `net_ns()`, `set_net_ns()` |
| `syscall/family.rs`, `syscall/namespace.rs`, `syscall/launch.rs` | `CLONE_NEWNET` in `namespaces_asked`, `give_namespaces`, `sys_unshare`; a native child takes its creator's |
| `fs/sockname.rs`, `fs/socket.rs` | abstract names keyed `(net ns id, name)`; a unix socket records its namespace's number |
| `fs/procfs.rs`, `fs/procfs/render.rs` | `/proc/<pid>/ns/net` (link and file); `/proc/net/*` from the reader's namespace |
| `fs/sysfs.rs`, `interfaces/net_ring/mod.rs` | `/sys/class/net` from the reader's namespace; device keys (§3.2) |
| `fs/netns_check.rs` (new), `fs/kmem_check.rs`, `stages_check.rs`, `panic/catalog.rs` | the `netns` line, FX-0893; three fills |

`/proc/<pid>/net` (the per-process view) is not built: `/proc/net` shows the
reader's namespace (Linux's `/proc/net` is `self/net`), and a program that
wants another process's goes through `setns` and then reads it. **Not built.**

---

## 5. F-37: every kernel object a program can make and keep is charged

| Kind | Made by | Charged |
|---|---|---|
| network namespace: itself and the vectors of its loopback | `CLONE_NEWNET` | at creation, to the caller's job: `arc_footprint::<NetNamespace>()` and 512 bytes |
| interfaces, addresses, routes, the neighbour cache and what it holds back, fragments | netlink, `ioctl`, ARP | the **tables charge**, a `Charge` to the job that made the namespace (`Charge::grow` charges the same job): `fit` makes it what `Stack::footprint` says. A request that adds first asks `admit`, which makes room for two more interfaces; the charge is then settled to what is held. A namespace that cannot pay gives up what it learned (`Stack::shed`) and tries again, and then refuses `ENOMEM`. Charged to the owner's job whoever asks, as the namespace is the owner's. **Two things grow on traffic and are not charged as they fill:** the neighbour cache (at most 256 entries, each holding up to 3 queued packets: about 1.1 MB per namespace) and the reassembler (32 KiB outside the first namespace); `fit` settles them to the job at the namespace's next change, and the namespace's own ceiling is what bounds them until then (consultant's review, 2026-10-01; BACKLOG) |
| a veth pair's record and its place in the table | `RTM_NEWLINK` | at creation, to the job of the caller: `veth::record_cost()`. The interfaces are the tables' |
| sockets, connections, queued datagrams | as today | as today (`socket_charge`, the stack's `owner`) |
| a netlink socket | `socket()` | as today |

An unprivileged user can now reach the netlink writers (by owning a network
namespace), which before they could not; the tables charge is what makes that
safe to allow, and the ceilings of §7 bound what a single namespace holds.
`kmem_check` fills three kinds: network namespaces made until `ENOMEM` (and
required to cost at least the structure each is), veth pairs (a pair alone
must cost at least its record and two interfaces; then made between fresh
namespaces until `ENOMEM`), and routes added to one namespace by netlink until
`ENOMEM` (so that the job's limit, not the route ceiling, ends it). Each is
refused by the limit, a sibling job then makes one, and both jobs read zero
after. The first namespace is charged nothing, as before.

---

## 6. Locks

| Lock | Kind | Guards | Order |
|---|---|---|---|
| `NetCore::stack` | `SpinLock` | one namespace's `Stack` | after nothing; **never two at once** (the forward loop takes them one after the other) |
| `NetCore::pending`, `transmit_wakers` | `SpinLock` | as today | after `stack`, as today |
| `LIVE` | `SpinLock` | the list of live namespaces | a leaf; cloned out before any tick |
| `PAIRS`, `Pair::ends` | `SpinLock` | the pair table, each pair's two ends | leaves; never held with a stack lock; released before a stack is entered |
| `DEVICES` | `SpinLock` | the device registry | a leaf, as above |
| `Process::net_ns` | `SpinLock` | the pointer | a leaf; cloned out |
| `tables` | `SpinLock` | the charge | a leaf; taken *after* a `look` of the counts has released the stack, and held across `Charge::resize`, which takes the quota account's locks as every charge does |
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

* Interfaces per namespace: 64; addresses: 256; routes: 1024 (`ENOSPC`). They
  apply to the first namespace as well, which nothing reaches. Each bounds
  what one namespace's tables can hold before the charge is asked.
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
  both, with the loopback down (the wildcard) and up; a listener in one is not
  reachable from the other over `127.0.0.1`; a datagram stays in the namespace
  that sent it; netlink and `/proc/net/tcp` show only the reader's.
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
  (`IFLA_LINKINFO` kind `veth`, peer and its namespace in `IFLA_INFO_DATA`),
  not running until both ends are up, a UDP datagram and a TCP stream cross it
  with the peer resolved by ARP; moved by `IFLA_NET_NS_FD`, the peer follows;
  `RTM_DELLINK` of one end deletes both.
* **NN9. Moving needs `CAP_NET_ADMIN` over source and target.** An unprivileged
  user namespace cannot push an end into the first namespace or another
  user's, nor make the peer of a pair there.
* **NN10. A physical interface stays in the first namespace unless a
  privileged caller moves it, and returns when the namespace ends.**
* **NN11. Abstract `AF_UNIX` names are per network namespace.**
* **NN12. `/proc/<pid>/ns/net` names the namespace and opens as it;
  `/proc/net/{dev,route,tcp}` and netlink are the reader's.**
* **NN13. A namespace ends with its last user.** Its veth ends vanish, peer
  included; its physical interfaces go home.
* **NN14. Charged (F-37):** namespaces, veth pairs and the routes of a
  namespace, to the job, refused `ENOMEM` at its limit.
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

`fs/netns_check.rs`, the `netns` line (FX-0893), registered beside
`check_user_namespaces` in `stages_check.rs` (1099 calls, 18 of them refusals,
the same on x86_64, aarch64 and armv7a). It makes processes that are root and uid 1000, drives
`unshare`, `socket`, `ioctl`, `openat` and the unix calls through the
system-call layer where a rule is about a call (NN5, NN6, NN7, NN11, NN12,
NN15), and netlink and the namespaces' own sockets where it is about what
crosses them (NN1 to NN4, NN8 to NN10, NN13, NN16, NN17), a request at a time
through `NetlinkSocket::send` with the acting process named. NN14's fills are
in `fs/kmem_check.rs` and print on the `kmem` line. Host tests in
`src/lib/network/net` cover the library half.

**Negative controls**, each a one-line sabotage, run and never committed, after
which the boot stopped with a message of the check's own. File, the change and
the message (`FX-0893` unless it says otherwise):

| Rule | Sabotage | Message |
|---|---|---|
| NN5 | `syscall/namespace.rs`: `held.holds(CAP_SYS_ADMIN).then(...)` becomes `true.then(...)` | an unprivileged process made a network namespace |
| NN5 | `syscall/family.rs`: the `CLONE_NEWNET` test in `namespaces_asked` becomes `if false` | clone with CLONE_NEWNET was not refused EPERM to an unprivileged process |
| NN15 | `syscall/family.rs`: `if false && flags & CLONE_NEWNET != 0 && flags & CLONE_THREAD != 0` | CLONE_NEWNET with CLONE_THREAD was not refused EINVAL |
| NN1 | `iface.rs`: the new loopback's flags gain `IFF_UP \| IFF_RUNNING` | a new network namespace did not start with a loopback that is down |
| NN2 | `stack.rs`: `if false && interface.addresses.is_empty()` | bringing a namespace's loopback up did not give it 127.0.0.1 and ::1 |
| NN2 | `stack.rs`: `interface.addresses.clear()` removed from going down | taking a namespace's loopback down left its addresses |
| NN3 | `net/socket.rs`: `open` ignores the namespace it is given and uses the first | the same port could not be bound in two namespaces |
| NN4 | `syscall/sockets.rs`: `socket()` opens in the first namespace | a socket made before its maker moved did not stay in its namespace |
| NN6 | `netlink/link.rs`: `net_admin` answers `true` | an unprivileged process changed the first network namespace |
| NN6 | `netlink/link.rs`: `capable_over(held, &held.user_ns, ...)` (the caller's own namespace, not the owner) | fake root in a user namespace changed the first network namespace |
| NN6 | `userns.rs`: `(1 << CAP_NET_ADMIN)` becomes `(0 << CAP_NET_ADMIN)` in `HONOURED` | a link could not be brought up by its owner |
| NN7 | `userns.rs`: `(0 << CAP_NET_RAW)` in `HONOURED` | the owner of a network namespace could not open a raw socket there |
| NN7 | `syscall/sockets.rs`: `if raw` becomes `if false` | FX-0701, stage 7's check, first: uid 1000 opened a raw socket |
| NN8 | `veth.rs`: `forward`'s budget is 0 | a datagram did not cross a veth pair |
| NN8 | `route.rs`: a veth end's `IFLA_LINKINFO` left out of a dump | a dump of links did not say a veth end is a veth |
| NN16 | `veth.rs`: `pair()` answers the first pair for every number | a frame sent on one veth pair arrived at the peer of another |
| NN9 | `link.rs`: the target's `net_admin` test becomes `false` | an unprivileged user namespace pushed a veth end into the first user namespace's |
| NN10 | `device.rs`: `come_home` forgets the device instead of placing it | a device was nowhere after the namespace it was in ended |
| NN10 | `namespace.rs`: a move does not place the device | the registry still named the old namespace after a device moved |
| NN13 | `namespace.rs`: a namespace's end leaves its veth ends | FX-0906, `kmem`'s check, first: a veth pair's namespaces are gone and their heap is still charged |
| NN11 | `fs/socket.rs`: every unix socket's namespace number is 0 | the same abstract name could not be bound in another network namespace |
| NN12 | `render.rs`: `/proc/net/dev` reads the first namespace | /proc/net/dev showed another namespace's interfaces |
| NN12 | `render.rs`: `/proc/net/tcp` reads the first namespace | /proc/net/tcp showed another namespace's sockets |
| NN12 | `netlink/mod.rs`: requests are answered from the first namespace | FX-0906, `kmem`'s route fill, first: a fill was refused by something other than its limit |
| NN17 | `route.rs`: the address ceiling becomes `if false` | a namespace took more addresses than its ceiling |
| NN17 | `route.rs`: the route ceiling becomes `if false` | a namespace took more routes than its ceiling |
| NN17 | `namespace.rs`: the interface ceiling is ten times higher | a namespace took more interfaces than its ceiling |
| NN14 | `namespace.rs`: a namespace is charged 0 | FX-0906: kmem: a network namespace was not charged to its job for itself |
| NN14 | `veth.rs`: a pair's record is charged 0 | FX-0906: kmem: a veth pair was not charged to its job for its record and ends |
| NN14 | `namespace.rs`: `admit` answers `Ok(())` | FX-0906: kmem: a fill was refused by something other than its limit |

Three controls are stopped by a check that runs before this one (a boot check
stops at the first failure): the unprivileged raw socket by stage 7's, the end
of a namespace leaving a pair by `kmem`'s, and a netlink request answered from
the wrong namespace by `kmem`'s fill of routes. Each is still stopped with a
message that names what is wrong.

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

**Built** on branch `stage13-netns` (2026-09-30) and reviewed by the
certification consultant (2026-10-01), whose conditions are met; the branch is
one commit on top of `main` after the pid namespaces.

* `CLONE_NEWNET` through `clone`, `clone3` and `unshare`, alone or with
  `CLONE_NEWUSER`; a namespace of its own stack (interfaces, addresses,
  routes, neighbours, ports, sockets, reassembly), owned by a user namespace,
  charged to the job that made it, ended with its last holder.
* A new namespace has a down loopback and nothing else; `RTM_NEWLINK` or
  `SIOCSIFFLAGS` with `IFF_UP` gives it `127.0.0.1/8` and `::1`; down takes
  them away.
* A process holds its namespace and a fork child, and a native child, takes
  it; a socket holds the one it was made in (inet, packet, netlink, and the
  unix socket's abstract names).
* Changes judged by `CAP_NET_ADMIN`, raw and packet sockets by `CAP_NET_RAW`,
  over the user namespace that owns the network namespace; `HONOURED` gains
  the two bits.
* veth pairs: `RTM_NEWLINK` with `IFLA_LINKINFO` kind `veth` and the peer in
  `IFLA_INFO_DATA`, each end placed by `IFLA_NET_NS_PID`; moved by
  `IFLA_NET_NS_PID` or `_FD`; `RTM_DELLINK`; a link dump says `veth`.
* A ring-3 NIC is served by a device key, moves with its interface, and comes
  home when its namespace ends; a namespace ending destroys its veth ends with
  their peers.
* `/proc/<pid>/ns/net` (link, and a file that `IFLA_NET_NS_FD` takes),
  `/proc/net/*`, `/sys/class/net` and netlink show the reader's namespace.
* The tables are charged (`Stack::footprint`, `shed`) and have ceilings.

**Evidence.** Ten host tests of the stack's half (`src/lib/network/net`,
`tests/namespaces.rs`; the crate's 82 pass). The `netns` boot line (FX-0893, `fs/netns_check.rs`): 1099 calls, 18
refusals, the same count on x86_64, aarch64 and armv7a at `--smp 2`. Three more fills on the `kmem` line. Thirty negative controls (§9),
each run on x86_64, three of them stopped by an earlier check with a message
of its own.

**Gated again for the landing (2026-10-01), through `fleet/gate.sh` on nazuna**
(the INDEX lines name the commit): on c14d6c69, `cargo xtask check`,
`test-boot` on x86_64 (KVM), aarch64 and armv7a at `--smp 2`, `test-init --arch
all`, and `test-shell`, `test-vfs` and `test-net --arch all` with ferrousli,
all passed (tags `ae19-*`). The controls of §9 and `pid_scope`'s were run as
`gate.sh control --expect` on 444344e4, whose kernel is c14d6c69's (the rebase
between them touched only generated documents), tags `ae-nn-01` to `ae-nn-31`:
each FIRED with its own message, except that NN3's open in the first namespace
stops at the same section's earlier refusal, "a port could not be bound in a
namespace" (`ae-nn-07b`), and NN5's `clone` control is `ae-nn-02b`.

**Gated first**, on the code of e8de326e (the commits after it are documents): `cargo
fmt` and `cargo clippy -p ferrix-kernel` on x86_64, aarch64 and armv7a; `cargo
test` of `ferrix-net` (82), `ferrix-linux-abi` (146) and `ferrix-netlink` (48);
`check-item-boundary.py`; on nazuna through the queue, `test-boot` on x86_64,
aarch64 and armv7a at `--smp 2` (the `netns` line on each, with `kmem`, `net`,
`netlink` and the ring check passing), `test-shell` and `test-vfs` on x86_64
with the static busybox, and `test-net` on x86_64 (all 17 network programs,
`wget`, `curl`, `nc`, `arp`, `/proc/net/dev`, TLS and `git`). The controls were
run on x86_64 on a Windows host (`cargo xtask test-boot`, QEMU under TCG), one
of them again on aarch64 through the queue (`link.rs`'s `net_admin` over the
caller's own namespace: "fake root in a user namespace changed the first
network namespace"). Not run: `cargo xtask check`, `test-init`, `carry-coverage`,
`gen-coverage-justification`, which are the integrator's.

**How it differs from the design above, where the building found it wrong:**

* The process holds an `Option`, `None` for the first, so that a process made
  during early boot does not make the net core before the clock.
* The check is in `fs/` (it uses the mount check's helpers, which are
  `pub(super)`) and not in `net/`.
* The namespace is a member of the process's `NsProxy` (`syscall/nsproxy.rs`,
  `net: Option<Arc<NetNamespace>>`, `None` for the first), not a field of
  `Process`: a fork, a native child and the other kinds' joins carry it with
  the UTS, IPC and cgroup ones.
* `IFLA_NET_NS_PID` names a process by the number the caller's pid namespace
  gives (`pidns::find_in`); the kernel's own checks speak kernel numbers
  (`pid_scope` in the `netns` line, from the consultant's review).
* `admit` makes room for two interfaces (`HEADROOM` = 1 KiB) before an add;
  the rest of the charge is settled after a change, and `tables_cover` is what
  `kmem_check` holds it to.
* `/sys/class/net` and the native child are the reader's and the creator's
  namespace: the design listed one and not the other.

**Where it differs from Linux** (beside the two honoured capabilities, which
Linux honours all of):

* A network namespace is a process's, not a task's: `CLONE_NEWNET` with
  `CLONE_THREAD` is `EINVAL`, and `unshare(CLONE_NEWNET)` from a process with
  more than one thread is `EINVAL`. (Not checked at boot: the harness makes no
  threads; the code is one line in `sys_unshare`.)
* A moved interface keeps no index (it gets the next free one) and a name the
  target has is `EEXIST`, where Linux renames it to `dev%d`; an interface that
  comes home to the first namespace is renamed `devN` on a clash.
* `ip link add type X` makes `veth` only; `dummy`, `bridge`, `macvlan` and the
  rest are `EOPNOTSUPP`. A pair's MTU is 1500, and it has no queue
  attributes, statistics or multicast groups (there are no notifications).
* The stack is a host, not a router: a namespace with two veth ends does not
  forward between them. **Container networking that wants NAT or a bridge is
  not built**, and needs forwarding in `src/lib/network/net` first.
* `setns(CLONE_NEWNET)`, `RTM_NEWNSID`/`RTM_GETNSID`, `IFLA_LINK_NETNSID`,
  `IFLA_TARGET_NETNSID` and `/proc/<pid>/net` are not built. `IFLA_NET_NS_FD`
  takes a descriptor of `/proc/<pid>/ns/net` (`net/netns_file.rs`, the smallest
  `nsfs`); the branch that owns `setns` has the general one.
* The ceilings (64 interfaces, 256 addresses, 1024 routes per namespace) also
  apply to the first namespace.
* A reassembler in a new namespace holds 32 KiB, not 256.

**Open.**

* **Merge, done.** `Process::net_ns` is an `NsProxy` member and `procfs.rs`'s
  `NamespaceKind::Net` is 7, after the pid kinds. Still open: `netns_file.rs`
  becomes `Handle::Net` in `nsfs.rs` when `setns(CLONE_NEWNET)` is built.
* **No limit on how many network namespaces exist.** Each is charged to its
  job, so a job's memory limit bounds them, but the net task ticks every one
  that exists (consultant's F3). A `max_net_namespaces` limit like N5's other
  sysctls would bound the ticking too; BACKLOG.
* The raw and packet socket code is now reachable by an unprivileged user who
  owns a namespace (VULNERABILITY-ANALYSIS); that is a larger attack surface
  than before, in safe Rust, with no ring-buffer interface.
* Not checked at boot: `unshare` from a multi-threaded process and
  `/sys/class/net`'s per-reader listing (a native child inheriting the
  namespace is checked, in `namespace_check`). A
  namespace ended by the last `close` of a socket, rather than the last
  process, is covered by the ownership rules and not by a check of its own.
* `tables_cover` and the ceilings make the tables safe to give a user; the
  neighbour cache (256 entries, each holding back three packets) is charged
  as it fills and shed when it cannot be paid for, but the wait to learn is the
  sender's. No check drives ARP past the limit.
