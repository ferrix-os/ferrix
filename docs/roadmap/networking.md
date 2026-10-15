# Networking — sockets, a net core, virtio-net ✅

Placed after stage 11 without a number of its own, the way *ARMv7-A* sits
after stage 4. The net core was named in `docs/ARCHITECTURE.md` and left
unstaged, because nothing on the path to `rustc` needs it. Then a sweep of
busybox's applets over stage 8's root showed how much of a real userland does:
`ifconfig`, `route`, `netstat`, `ip`, `wget`, and everything that talks over a
local socket, failed then, because stage 7 refused every socket call. All of
them run under `cargo xtask test-net` now.

The net core sits beside the block core. It provides sockets in the Linux
ABI: `AF_UNIX` stream and datagram with descriptor passing (pulled forward to
stage 17's path on 2026-09-13, because Wayland is an `AF_UNIX` socket
carrying descriptors, and needing no net core), `AF_INET` and
`AF_INET6` TCP and UDP, and the `AF_NETLINK` route family that `ip` configures
interfaces through. Those run over interfaces, routes and a loopback device.
virtio-net is the first driver. It runs in user mode on stage 10's device
objects and speaks to the core over a channel, with its buffers in VMOs, as
virtio-blk speaks to the block core. `/proc/net` (`arp`, `dev`, `route`,
`tcp`, `tcp6`, `udp`, `udp6`) comes with it, rendered in `src/lib/fs/procfs` like
the rest of `/proc`; `unix` is not there yet.

The byte-level halves are `src/lib/` code, host-tested and fuzzed before the
kernel calls them, for the reason the continuous rule gives: a packet is bytes
someone else chose. They are header parsing, the TCP state machine with its
retransmission and congestion arithmetic, and netlink message encoding.

**Written ahead, before the net core landed on 2026-09-16** (the counts are
of that day). `src/lib/network/netwire` has the headers — Ethernet, ARP,
IPv4 and IPv6 with its extension headers, ICMPv4, ICMPv6 and Neighbor
Discovery, UDP and TCP — with 54 host tests and the `netwire_parse` fuzz
target; see *Written ahead of their stage*. `src/lib/proto/linux-abi` has the numbers
and layouts a program passes: `sockaddr_in` and `sockaddr_in6`, the
`IPPROTO_`, `IP_`, `IPV6_` and `TCP_` options, and the fixed headers of
netlink and its routing messages, each checked against a probe compiled from
the UAPI headers. `src/lib/network/nettcp` has the TCP state machine over those headers,
with 30 host tests and the `nettcp_state` fuzz target, and `src/lib/network/net` has the
net core over both — interfaces, routes, neighbours, reassembly, ICMP, UDP and
the socket table — with 45 host tests and the `net_input` fuzz target; see
*Written ahead of their stage*. `src/lib/network/netlink` has the byte-level half of
netlink over those headers — walking a buffer of messages and the attributes
after each one, and building replies into a caller's buffer — with 48 host
tests and the `netlink_walk` fuzz target. virtio-net is written too, in the two halves
virtio-blk is split into: `src/lib/drivers/virtio`'s `net` module for the device protocol
— the configuration block, the feature bits and the header — and
`src/lib/drivers/net/virtio-net` for the driver logic over two queues, with 22 host tests and
the `virtio_net` fuzz target. The kernel calls all of it now, below.

**Done — the net core, and `AF_INET` and `AF_INET6` sockets.** `src/kernel/src/net`
is `src/lib/network/net` behind one lock and a task that drives it. Nothing sleeps inside
that lock: every call takes what it needs into a kernel buffer, drops it, and
only then touches the program's memory, which is `src/kernel/src/fs/socket.rs`'s
rule and the same reason. Sockets have no wait queue of their own -- they all
wait on one, woken whenever the stack moved, and each waiter re-checks its own
condition; that is a thundering herd in the textbook sense and the right trade
for a host with tens of sockets rather than thousands.

`socket`, `bind`, `listen`, `accept`, `accept4`, `connect`, `getsockname`,
`getpeername`, `send*`, `recv*`, `shutdown`, `getsockopt`, `setsockopt` and
the two queue ioctls answer for `AF_INET` and `AF_INET6` as they already did
for `AF_UNIX`, through one enumeration so the order of Linux's checks cannot
drift apart between the families. An `AF_INET6` socket carries IPv4 through
`::ffff:0:0/96` unless `IPV6_V6ONLY` says otherwise, and reports a v4 peer in
that spelling. `SOCK_RAW` was `EPERM` for everyone until raw sockets landed
(below); it still is for a process without `CAP_NET_RAW`.

The boot check uses the loopback and nothing else, so it passes on a machine
with no network device -- which every machine was until the driver landed. It
requires a datagram to arrive with its sender's address, a datagram to an empty
port to earn `ECONNREFUSED` from the unreachable this host sends itself, a
connection to be made, accepted, to carry bytes both ways and to end as a clean
close, and a connection to a port nobody listens on to be refused rather than
left to time out -- over IPv4 and again over IPv6. It reads:

```
  net      1 interface up, 318 bytes carried over the loopback in both families, 2 connections made and accepted, 3 calls refused as specified
```

**Done — raw IPv4 sockets, for `ping`.** busybox 1.37's `ping` opens
`socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` and nothing else — it has no fallback
to the unprivileged echo socket — and read every reply with its IPv4 header in
front. `SOCK_RAW` now opens for root at any protocol from 1 to 255 (zero is
`EPROTONOSUPPORT` for everyone, as `inet_create`'s lookup makes it; past
`IPPROTO_MAX` is `EINVAL`), and is `EPERM` without the privilege. A raw socket
is handed a copy of every IPv4 packet of its protocol that reaches this host,
header and all, beside the stack's own handling: an echo request is both
answered and copied. It takes only its peer's packets once connected, only its
address's once bound, and only its device's once pinned; `IPPROTO_RAW` takes
nothing; `ICMP_FILTER` holds back the ICMP types it names. A send builds the
header, or with `IP_HDRINCL` — always set for `IPPROTO_RAW` — completes the
program's: the length, the checksum, and an identification and source left at
zero. `AF_INET6` raw sockets and `AF_PACKET`, which `udhcpc` needs, came
next, below. The boot check pings the loopback through a raw socket and
requires the request and the reply, each with its header; a second socket
filtering replies must read the request and not the reply; an `IPPROTO_RAW`
packet with zeroed fields must reach a UDP socket; and uid 1000 must be
refused, a control that panics the boot when the privilege check is removed.
The line now ends `, 3 packets read by raw sockets`, and `test-net`'s `ping`
passes with the busybox built against ferrousli.

**Done — `AF_PACKET` sockets, for `udhcpc`.** A DHCP client has to hear an
offer for an address its interface does not have yet, which IP drops, so it
listens below IP: busybox's `udhcpc` opens `socket(AF_PACKET, SOCK_DGRAM,
htons(ETH_P_IP))`, binds it to the interface with a `sockaddr_ll`, and reads
IPv4 packets with the link header taken off; it sends through another, naming
the broadcast hardware address. Packet sockets now open for root, in both
types: `SOCK_RAW` reads and writes whole frames, `SOCK_DGRAM` payloads with
the stack putting the header on. Each is handed a copy of every frame of its
protocol, or of every protocol with `ETH_P_ALL`, that an Ethernet interface
takes in, on the interface it is bound to or on all of them, with the
sender's hardware address and whether the frame was to this host, the
broadcast or a group. `packet_create` asks for the capability before the
type, and that order is kept: uid 1000 is `EPERM` even for a stream packet
socket. Not yet: frames this host sends are not copied back to `ETH_P_ALL`
sockets, a packet socket on the loopback carries nothing, `SOL_PACKET`
options are `ENOPROTOOPT` — `udhcpc`'s `PACKET_AUXDATA` among them, which it
takes quietly — and no BPF filter attaches, which busybox 1.37 compiles out.
The net ring's boot check binds a packet socket for ARP to its pretend
driver's interface: it must read the ARP request the driver delivers, with a
`sockaddr_ll` naming the sender, the interface and the broadcast, and a frame
it sends to the peer must be the next one the kernel submits, with the link
header it asked for. With the stack's frame tap removed, the boot panics with
"a packet socket bound for ARP did not read the ARP request". The line ends
`, 2 through a packet socket`. In a `run --net` guest on Windows, `udhcpc -i
eth0 -n -q` broadcast its discover and select, got the lease of 10.0.2.15
from the gateway, and the address, route and resolver it configured fetched
`http://example.com`. Since the landing after it, the image carries that
`default.script`, the interactive shell's `/etc/profile` runs `udhcpc` when
`eth0` has no address, and `test-net` gets its address the same way: its
second program is `udhcpc -i eth0 -n -q`, which must print the lease the
gateway gave.

**Done — raw IPv6 sockets, for `ping6`.** busybox's `ping6` opens
`socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6)`, requires `ICMP6_FILTER` to be
accepted, asks for `IPV6_CHECKSUM` at `SOL_RAW`, and prints the hop limit from
a control message. `SOCK_RAW` now opens in `AF_INET6` too, on the same terms
as in `AF_INET`. An IPv6 raw socket reads what follows the header and its
extensions, not the header, as RFC 3542 says. It matches peer, address and
device as an IPv4 one does. A message whose checksum at the socket's
`IPV6_CHECKSUM` offset does not verify is dropped, and `ICMP6_FILTER`'s eight
words hold back the ICMPv6 types they block. On send the stack writes that
checksum over the pseudo-header, always at offset 2 for ICMPv6. The option is
`EINVAL` at `SOL_IPV6` on an ICMPv6 socket and for an odd offset, and a
negative offset turns it off. `recvmsg` now writes control messages as
`put_cmsg` does, with `MSG_CTRUNC` for one that does not fit. The first ones
are the hop limit: `IPV6_RECVHOPLIMIT` gives an `IPV6_HOPLIMIT` message and
`IPV6_2292HOPLIMIT`, which musl hands `ping6`, one of that older type, on any
IPv6 datagram socket. Every received datagram now carries its hop limit. The
stack's echo replies over IPv6 went out with hop limit 255, the Neighbor
Discovery value, and now carry the stack's default of 64, as Linux's do. Not
yet: `IPPROTO_RAW` in IPv6 opens but its sends are `EINVAL`, because the
program's own IPv6 header is not taken. The boot check pings `::1` through a
raw ICMPv6 socket with the checksum left zero. It requires the reply without
a header, with a checksum that verifies and with `IPV6_2292HOPLIMIT` 64. A
second socket that passes only requests must read the request and not the
reply. With the filter disabled, the boot panics with "ICMP6_FILTER let the
echo reply it blocks through". The line now ends `, 7 calls refused as
specified, 6 packets read by raw sockets`, and `test-net` runs `ping6 -c 2 ::1`,
which must print `ttl=64`: a stack that sent no control message would print
-1.

**Done — `AF_NETLINK` route sockets, which is how an interface is
configured.** Every way of configuring a network on Linux ends at the same
socket: `ip` uses nothing else, `ifconfig` and `route` use ioctls that are a
shim over it, and `udhcpc` and a C library's `getifaddrs` read it directly.
`socket(AF_NETLINK, SOCK_DGRAM | SOCK_RAW, NETLINK_ROUTE)` now opens one,
`bind` gives it a port identifier, and `sendmsg` and `recvmsg` carry requests
and replies.

`src/kernel/src/net/netlink` answers dumps of links, addresses, routes and
neighbours, and the changes that matter: `RTM_SETLINK` and the `RTM_NEWLINK`
that `ip link set dev eth0 up` actually sends, `RTM_NEWADDR` and `RTM_DELADDR`,
`RTM_NEWROUTE` and `RTM_DELROUTE`. An unknown type is `NLMSG_ERROR` with
`EOPNOTSUPP`, a message too short for the fixed header its type implies is
`EINVAL`, and a change asked for without `NLM_F_REQUEST` is `EINVAL` — a
notification is what the kernel sends, not what it takes.

A request is answered before `sendmsg` returns, as `NETLINK_ROUTE` is on
Linux, which is what lets `rtnl_talk` send and then read with no poll and no
timeout. Each reply is queued as a datagram of its own rather than packed with
its siblings, because a netlink datagram that does not fit the buffer offered
is truncated and the rest dropped — one dump in one datagram would be a reader
with a small buffer silently losing interfaces. Nothing is encoded inside the
net core's lock: the buffer is allocated before it is taken and the replies
copied out after it is dropped.

What is not there is multicast — nothing yet sends a notification when an
interface changes — and dump filters: a `RTM_GET*` answers with the whole table
whether or not `NLM_F_DUMP` was set, because the attributes that would narrow
it are read by nobody. Both wait for the first program that needs them.

The boot check is the path `ip` takes rather than the pieces it is made of: it
opens a socket, binds it, reads its port back, dumps the links and finds the
loopback with its flags, adds an address and a route and sees each in the next
dump, removes them and sees them gone, and requires `EOPNOTSUPP` for a type
nothing answers and `EINVAL` for a message too short for its header. It reads:

```
  netlink  1 links, 2 addresses and 2 routes dumped, an address and a route added and taken away again, 2 requests refused as specified
```

**The host side already exists, and it is ours.** `cargo xtask run --net`
attaches a virtio-net device whose backend is `tools/common/xtask/src/gateway/`: a NAT
gateway in the build tool, on the guest network `10.0.2.0/24` with the gateway
at `10.0.2.2`, DNS at `10.0.2.3` and the guest at `10.0.2.15` — slirp's numbers,
so that every habit and every piece of QEMU documentation carries over. It
answers ARP and ICMP echo for its own addresses, offers the guest its address
over DHCP, relays UDP through one ephemeral host socket per flow with
`10.0.2.3:53` forwarded to the host's resolver, and terminates TCP, re-opening
each connection as an ordinary host `TcpStream`. Eighteen host tests speak to it
over the socket QEMU would use, so the half of the path that is ours is
covered by `cargo test` with no QEMU and no network at all.

**Why it is written rather than QEMU's own.** `-netdev user` is slirp, and
slirp is an optional build-time dependency: the QEMU this was developed against
was built without it, and says so — *network backend 'user' is not compiled
into this binary*. The two ways round that both want privilege a build tool
should not ask for. `-netdev tap` needs `CAP_NET_ADMIN` or a setuid helper, and
the usual escape — a `tap` inside an unprivileged user namespace — is refused
outright on a host whose `AppArmor` policy blocks those namespaces, as Ubuntu's
now does. What is always available is QEMU's `dgram` backend, which hands every
Ethernet frame to a datagram socket; the other end is a network backend anyone
can write, and this is it. It was a UNIX datagram socket at first, and since
2026-09-16 it is UDP on the loopback, because Windows has no datagram
`AF_UNIX`. No raw socket, no tun device, no capability, and the same behaviour
on every developer's machine.

ICMP echo to the outside was not forwarded at first, because originating ICMP
needs a raw socket or a permitted ping group. Since 2026-09-16 it is
(`tools/common/xtask/src/gateway/icmp.rs`): through `IcmpSendEcho` on Windows, an
unprivileged ICMP socket where the ping group admits the user, or the host's
own `ping` where neither does. What it does not do is IPv6, because a
half-answered IPv6 is worse than none — a guest that receives a router
advertisement will prefer the address in it.

**Done — the ring the driver will speak over.** `src/lib/proto/netring` and
`docs/NET-RING.md` are the memory the kernel shares with a ring-3 network
driver. It is the block ring's discipline with its allocator taken out: a frame
is bounded by the interface's MTU, so the data VMO is `entries` slots of a fixed
size and a submission names its slot. That removes the whole region-allocation
half of the protocol and with it the class of bug where a region is reused
before its completion, which on an untranslated IOMMU domain is a device writing
into somebody else's packet. The index discipline is `src/lib/proto/blkring`'s, written a
second time rather than shared, which `docs/BACKLOG.md` carries as a debt with
its reason.

**Done — the kernel's end of the ring.** `src/kernel/src/interfaces/net_ring` is one task per
ring: it waits for the driver's HELLO, checks the rights every handle carries
exactly rather than at least, holds the two VMOs, adds the interface to the net
core, and answers READY with its completion port. Then it posts half the ring
for the driver to fill and keeps the other half for frames the net core wants
sent — posting *every* free slot is the mistake that leaves an interface
receiving for ever and never answering, and the first end-to-end check of this
path found it.

The check plays the driver, so the whole kernel side runs on a machine with no
network adapter: it makes the VMOs and the port a driver makes, sends HELLO,
and answers submissions by hand. An ARP request written into a posted slot
comes back as an ARP reply in a slot the kernel submits, which is a frame in
and a frame out through the whole stack. It reads:

```
  netring  1 HELLOs refused as specified, 4 slots posted for a driver to fill, 1 frames taken up the stack and 1 answered back down it
```

**Done — `/proc/net`.** `src/lib/fs/procfs` gains `dev`, `route`, `tcp`, `tcp6`,
`udp`, `udp6` and `arp`, each pinned in its tests against a line copied from a
running Linux, because `route`, `netstat`, `arp` and `ifconfig` read these
files with `sscanf` and fixed columns and a field one column off is a program
that reads the wrong number confidently. Two details that look like mistakes
and are not: the addresses are the network-order bytes read as a host-order
number, so `10.0.0.0` prints as `0000000A`; and the lines are padded to a
fixed width, 127 for `route` and `udp` and 149 for `tcp`, by Linux's
`seq_pad`, which pads a short line and leaves a long one alone -- which is why
an IPv6 row overflows.

**Done — the driver, in ring 3.** `src/user/system/native/drivers/net/virtio-net` is the process that makes a
virtio-net function an interface. It holds handles and nothing else:
`src/lib/drivers/net/virtio-net` drives the device, `src/lib/proto/netring` speaks the ring,
`src/lib/drivers/net/netserve` joins the two, and all three are tested on the host, so the
program is the protocol of `docs/NET-RING.md` §7, with a `Step` exit code for
each of its eight ways to fail.
`devmgr` starts it from a second row in its table, and the net ring's `take_up`
sends PUBLISHED for the device's PCI location before READY goes out, because a
driver that has not published by the time `devmgr` reports is killed.

The first end-to-end run found the failure the ring's sleep handshake exists to
prevent: the driver waited on its port without first asking to be rung, so the
kernel — which rings only a driver that has said it is going to sleep — never
rang it. Frames the *device* delivered still woke it through the interrupt, so
the interface looked alive and transmitted nothing at all.

That was possible because `src/lib/drivers/net/netserve` left the handshake to its caller
while `src/lib/drivers/block/blkserve` owns it, which is why `src/user/system/native/drivers/block/virtio-blk` never had the bug and
`src/user/system/native/drivers/net/virtio-net` did. The handshake is now `netserve`'s too, and with it the rule
`blkserve` already had: while a frame waits for room in the device's transmit
queue the answer is always to sleep, whatever the ring holds. Without that
rule a full transmit queue is a spin rather than a wait — the loop takes no
submission while a frame waits, so the ring stays full and answers "do not
sleep" until the device interrupts. A test pins both.

**Done — the `ifreq` ioctls.** rtnetlink is how an interface is configured and
`src/kernel/src/net/netlink` answers it, but `if_nametoindex` — which POSIX.1-2024
specifies, which every program that names an interface goes through, and which
musl implements as `ioctl(SIOCGIFINDEX)` over an `AF_UNIX` socket — had nothing
to talk to, so `ip` could not find a device that was right there.
`src/kernel/src/net/ifreq.rs` is the index, the flags, the address, the mask, the
broadcast and peer addresses, the MTU, the hardware address, the queue length
and `SIOCGIFCONF`. `sys_ioctl` sends what a socket's own family did not know to
it whatever the family, as Linux's `sock_ioctl` passes it to `dev_ioctl`, so
`ifconfig` and `getifaddrs` are answered as well as `ip`.

**Done — `cargo xtask test-net`, the exit criterion as a test.** The servers
the guest fetches from are threads of `xtask` on ports the host's kernel chose,
and `10.0.2.2` is the host's loopback as it is under slirp, so the run is
hermetic: it says the same thing on a machine with no network, and the name it
resolves is answered by a stub the gateway's forwarder is pointed at for the
run. The digest is POSIX `cksum`, written out in `tools/common/xtask/src/net.rs` and checked
against what the host's own `cksum` prints, because it is the one digest this
busybox and this build tool can both compute with nothing added to either.

Thirteen programs, on x86-64, AArch64 and ARMv7-A: `ip` configures an address
and a route and reads them back; `route -n` and `netstat -rn` report through
`/proc/net/route`; `ping` reaches the gateway; `nslookup` resolves a name;
`wget` fetches by name through `/etc/resolv.conf` and fetches a quarter of a
megabyte whose `cksum` matches the server's; `nc -u` sends a datagram and reads
the answer; and `/proc/net/dev` and `arp -n` show what the traffic left behind.

**Done — `AF_UNIX` names.** `bind`, `listen`, `connect` and `accept` on a
local socket, over both namespaces Linux has. A pathname is a node in the
filesystem: `bind` creates an `S_IFSOCK` node exactly as `mknod` would, and
`src/kernel/src/fs/sockname.rs` maps that node — its device and inode numbers, not
the path, because two paths can name one node — to the socket. `connect` walks
the path like any other, which is what makes the permissions on the
directories above it mean something. An abstract name, a `sun_path` starting
with a NUL, is a flat namespace of its own that goes when the socket does. The
tables hold weak references, so a socket is not kept alive by having a name.

The connection is complete when it is queued rather than when it is accepted,
as Linux's `unix_stream_connect` has it, so a client may write before the
server calls `accept`. A socket left behind by a program that died keeps its
node — Linux does not unlink one either, which is why `unlink` before `bind`
is the universal idiom — and a `connect` to it is `ECONNREFUSED` rather than
`ENOENT`: the two answers say different things, and a C library reads them.

Which is how this closed the musl busybox's `su`, open since the applet
landed. musl's `initgroups` tries an `AF_UNIX` connection to nscd before it
reads `/etc/group`; `EOPNOTSUPP` is an error it gives up on, and `ENOENT` is
one it falls back from.

**Done — curl, built against ferrousli.** The curl port, now the `curl` app, builds
curl 8.22.0 over Mbed TLS 3.6.7 as a static x86-64 program against ferrousli,
from sources pinned by checksum, with curl.se's extract of Mozilla's CA
certificates. It linked with nothing missing from the library. `cargo xtask
ports` builds it. Every x86-64 image that carries a busybox carries it at
`/bin/curl`, with the bundle at `/etc/ssl/certs/ca-certificates.crt`. When
curl is installed, `test-net` adds two programs to its thirteen: curl fetches
the file by name through `/etc/resolv.conf`, and fetches the quarter megabyte
whose `cksum` must match the server's.

HTTPS first failed in the guest. A fetch from `https://1.1.1.1/` got through
the handshake to the certificate check and was refused with *"The certificate
validity starts in the future"*, because the kernel's clock started at 1970.
And `getrandom` was xorshift seeded from a counter, so a session key would have
been predictable.

**Done — the time of day, and random numbers worth a key.** The loader asks
firmware for both before it leaves boot services: `GetTime`, turned into Unix
nanoseconds by `ferrix_bootinfo::unix_nanos`, and 32 bytes from
`EFI_RNG_PROTOCOL`. `BootInfo` version 5 carries them, with a flag for each
that firmware provided. The kernel starts `CLOCK_REALTIME` at that time after
stage 3's timer check.

`src/lib/kernel/crng` is the generator: ChaCha20 with fast key erasure, its block
function checked against RFC 8439's vector and OpenSSL's keystream. Every
64-byte block replaces the key with its first half and hands out the second.
`src/kernel/src/random.rs` seeds it from firmware's bytes, credited 256 bits, and
from the CPU's `RDSEED` or `RDRAND` on x86-64, or `RNDR` on AArch64, at 32 bits
a word. On AArch64 it also asks firmware's True Random Number Generator
through SMCCC (`TRNG_RND64`, Arm DEN0098) for 48 bytes, credited in full, once
PSCI and SMCCC 1.1 say it is safe to ask (`src/kernel/src/arch/aarch64/trng.rs`).
It also mixes in timer jitter, credited nothing, and mixes the counter
into every read. `getrandom`, `/dev/random`, `/dev/urandom` and `AT_RANDOM` all
read it. A boot says what it had:

```
  firmware clock read, random number protocol read
  clock    1789590336 seconds since the epoch, from firmware's clock
  random   seeded with 512 bits: 32 bytes from firmware, 0 from its TRNG, 8 words from the CPU, timer jitter
```

That was OVMF with RDRAND turned on in `xtask`'s QEMU CPU. AAVMF, and U-Boot's
EFI on ARMv7-A, gave both the time and the random bytes too. A machine with no
firmware protocol and no CPU instruction boots `NOT SEEDED`, in capitals, and
`getrandom` answers anyway. Linux would block instead, but that wait never ends
on a machine with nothing to wait for. `BootInfo` version 6 adds how many bytes
firmware gave, and the kernel credits 8 bits for each, up to 256. The Pixel 7's
loader passes on the 8 bytes ABL leaves in `/chosen`, and its cores have no
`RNDR`, so the phone booted `NOT SEEDED: 64 of 256 bits` until its TF-A was
asked: it answers `TRNG_RND64`, as Android's `smccc_trng` driver reads it,
and the phone now boots `seeded with 448 bits: 8 bytes from firmware, 48 from
its TRNG`. QEMU's firmware has no TRNG and answers 0. The boot check
reads the generator twice and panics as FX-0306 if the two reads match. Its negative control, not
committed, on x86-64: with the second read replaced by a copy of the first, the
boot printed `FERRIX-PANIC random generator check failed: two reads of the
random generator were the same` under FX-0306. The clock's: with the loader's
time flag cleared, the boot said `firmware has no clock: CLOCK_REALTIME starts
at the epoch`.

`test-net` now has an HTTPS program that needs neither the internet nor the
host. Mbed TLS's own test server, built by the curl port, listens on the
guest's loopback with its certificate for `localhost`. curl fetches from it
trusting the test CA, and must see the server's page. It fetches again trusting
only the Mozilla bundle, and must be refused with 60. The certificate is valid
from 2023, so a guest whose clock came from anywhere but firmware fails the
first fetch.
Its negative control, not committed, on x86-64: with the loader's time flag
cleared, the guest printed `verified 0` and `untrusted 60`, and `test-net`
failed on that program.

**Done — git, built against ferrousli.** `src/user/system/linux/ferrousli/tools/ports/zlib` builds
zlib 1.3.2 and the git port, now the `git` app, builds git 2.55.0 over it and over
the curl port's libcurl and Mbed TLS. It is built without Perl, Python, Tcl,
gettext and iconv, which the image does not have. Its Rust half is off too: cargo
builds that for the host, against glibc. It uses git's own regex, because
ferrousli's, like musl's, has no `REG_STARTEND`. The library lacked `utime`
and `sync_file_range`. The image carries `/usr/bin/git` with `/bin/git`
linking to it, `git-core` with its links, and the templates, which `xtask`
now copies as trees. The servers and the scripts that need an interpreter are
left out.

`test-net` gains a git program with no network outside the guest. It makes a
repository and a commit, bare-clones it and clones that back over git's
local transport, then clones a repository `xtask` serves over the dumb HTTP
protocol at `10.0.2.2`, through `git-remote-http` and libcurl: neither
busybox on the image has `httpd`. The file and the commit's subject must
come back both times.

**Done — btop, and the C++ runtime under it.** `src/user/system/linux/ferrousli/tools/ports/libcxx`
builds LLVM 23.1.1's libc++, libc++abi and libunwind against ferrousli with
the host's gcc. The btop port (an app since 2026-09-30) builds btop 1.4.7, a C++23
program, over them. What ferrousli lacked for that landed with them:
`dl_iterate_phdr` and `dladdr`, the message catalogues, the `strtod_l` family,
`pathconf`, `copy_file_range`, `getloadavg`, and thread cancellation, which
btop uses to stop a stalled collector thread. The kernel lacked two things.
`/proc/<pid>/mounts` did not exist, and it is where btop reads the mounts.
And a program larger than four mebibytes could not be started at all: `execve`
read the file into one heap allocation, the heap takes a large one from the
buddy allocator in a single block, and the largest block is `2^MAX_ORDER`
frames. btop is 4.6 MiB, and `timeout btop` failed with `ENOMEM`. A program
is now read into a `vmap::Buffer`, on single frames mapped into the kernel's
arena, so the limit is `READ_FILE_LIMIT`'s 64 MiB, which it was always
documented to be. Since 2026-09-24 a program is not read at all: `execve` maps
it from its file and reads its pages when they are touched, so there is no
limit left but memory (`docs/CHROME.md` §2.2).

Every x86-64 image with a busybox carries `/bin/btop`. In a `test-net` guest,
`timeout 8 btop` drew the CPU, memory, network and process panels on the
serial console, with `eth0`, `lo` and the running `sh`, `busybox` and
`btop`, and redrew them every two seconds until `timeout` ended it. No gate
runs it yet; `docs/BACKLOG.md` has the row.

**Done — btop's keys on a pseudoterminal (2026-09-27).** In the desktop's
terminal btop ignored `q` below 80x24 and froze once on a retile. A slave's
raw read waited for input whatever `VMIN` and `VTIME` said, and btop reads
with both at zero until a read gives 0, so its second read never ended. The
slave now decides as the console does, in `Discipline::read_step`, which
follows Linux's `n_tty_read`. The syscall self-check holds every case, and
fails by name if `VMIN` is ignored again. Run as the boot's program on a pty,
the image's btop shows "Terminal size too small" at 77x21 and quits on `q`,
both when started that small and after a resize, and still redraws after a
key at full size.

**Done — an SSH server, and a way in from the host.**
The sshdt port, now the `sshdt` app, builds sshdt 0.4.2, an SSH server written in Rust
(russh, tokio, and aws-lc underneath). It is built the way uutils is: the musl
target, ferrousli in the C library's place, and aws-lc compiled with
`ferrousli-cc`. It linked on the first try, with nothing undefined. sshdt was
chosen over narrowd, which sandboxes itself with seccomp and Landlock, and over
russh and sunset, which are libraries with no daemon. `--forward <host>:<guest>`
gives `xtask`'s gateway the client half of TCP it lacked: it accepts on the
host's loopback, sends the guest a SYN from `10.0.2.2`, and relays the
connection like any other once the guest's SYN-ACK arrives. Three gateway tests
cover it: a forward carried both ways and closed, a guest that refuses one, and
a forward before the guest has spoken. The negative control, not committed:
with the SYN-ACK no longer opening the connection, the first of the three fails
waiting for the guest's bytes.

A KVM boot on example with `--forward 22022:22` ran `sshdt -b 0.0.0.0 -p 22`
with a public key, and the host's OpenSSH ran `uname -a` and `id` over it and
opened a session on `/dev/pts/0`. The gateway counted `2 forwarded`. Two things
showed up that are not SSH's: sshdt warns once that `mlock` is `ENOSYS`, and
uutils' `tty` on a PTY prints its name without the newline, where busybox's
`tty` prints both. No gate runs sshdt yet; `docs/BACKLOG.md` has the rows.

Who may log in was that host user's `~/.ssh` and nothing else, and on
2026-09-21 that was reported as a server refusing non-interactive sessions.
It refuses no such thing: from a client with a key, `uname -a`, an exit
status of 3, a piped stdin, stderr on its own stream, `ssh -T`, `sftp` and
`scp` all work over this guest. What the report had met was a client with no
private key at all -- a sandbox whose `~/.ssh` was empty -- being told
`Permission denied (publickey)` before any session was opened. So a machine
now has a key of its own for its guests: `ssh-keygen` makes
`~/.local/share/ferrix/ssh/id_ed25519` beside the host key the first time,
every `--ssh` boot authorizes it, and the boot prints the `ssh -i` line that
uses it. `--ssh-key <FILE|KEY>` authorizes a key that is in neither place, a
file read whole or a key written out. Password authentication stays off,
because `sshdt` given no key and no password accepts anyone. Proven from a
client with an empty `~/.ssh`, with both controls firing: no key and a key
that was not named are each refused.

**Done — curl and git on AArch64 and ARMv7-A.** The ports were x86-64 only;
the DK1's terminal answered `command not found: git`. With ferrousli ported to
both Arm targets, `tools/ports/common.sh` takes `--arch aarch64|armv7a`, as
busybox's build does: gcc for the target and its binutils, Alpine's pinned
`linux-headers` for the UAPI, and a build directory per architecture beside
the one install directory each has. zlib, curl with Mbed TLS, and git build
unchanged apart from their cross switches, with nothing undefined, and
`cargo xtask ports --arch <arch>` builds those three there. btop, libc++ and
sshdt stay x86-64 only: the first two need the target's g++, which the build
machine lacks, and sshdt's build names its Rust target. Every Arm image with a
busybox now carries the three, so `test-net --arch all` runs curl over HTTP
and HTTPS and git's clones on all three architectures.

ARMv7-A's first run failed every curl with *"A libcurl function was given a
bad argument"*, and git's HTTP clone could not connect, while the same
binaries worked under qemu-user. The kernel read the 64-bit `tv_nsec` of a
`__kernel_timespec` whole on a 32-bit build. Linux's `get_timespec64` keeps
only its low half there, because the upper half is padding a 32-bit program's
libc need not write, and ferrousli's `struct timespec` leaves it unwritten.
So every `ppoll` with a timeout failed with `EINVAL`. `time::read_pair` already
had Linux's rule; `ppoll`, `pselect6`, `rt_sigtimedwait` and `futex` each had
a reader of their own and now use it, and `utimensat` keeps the low half
too. The stage 7 check stages such a timeout with `0xDEADBEEF` above
`tv_nsec`. Its negative control, not committed: with `read_pair` taking the
field whole again, the ARMv7-A boot failed that check.

**Done — a socket's owner, and `accept4`'s flag (2026-09-28).**
`/proc/net/tcp`, `udp`, `tcp6` and `udp6` print the socket file's own inode,
the one `/proc/<pid>/fd` links it by, and the uid that opened or accepted
it, where they printed the net stack's id and 0: `lsof -i`, `ss -p` and
`netstat -p` can now say which process holds a connection, which the Steam
client asks of its web helper's websocket. And an accepted socket is
non-blocking only when `accept4`'s `SOCK_NONBLOCK` says so; the listener's
`O_NONBLOCK` decides only whether `accept` waits, as on Linux. It had been
given the listener's flag, so a server that accepted on a non-blocking
listener and then read with a timeout saw `EAGAIN` before the client's bytes
arrived. The boot checks ask both families.

**Written ahead — the N150 box's wired port, a Motorcomm YT6801
(2026-10-15, branch `net-yt6801`).** The box's gigabit port is a YT6801, PCI
`1F0A:6801`: a Synopsys DesignWare Ethernet QoS core ("dwmac4") at offset
`0x2000` of BAR 0, a YT8531S PHY inside the chip at MDIO address 0, and
Motorcomm's own registers in BAR 0's second page -- the PHY's reset, an eFuse
holding the MAC address, and an out-of-band wake-on-LAN block that holds the
DMA's interrupts back until it is disabled. The facts are from Linux's
`dwmac-motorcomm.c` and Motorcomm's series posted to netdev (v4); no code was
taken from either. It is three crates and a kernel change:

* `src/lib/drivers/net/dwmac` (`ferrix-dwmac`): the core as logic over
  registers, a clock and DMA memory -- software reset, the PHY over the core's
  MDIO in clause 22, one channel and one queue each way with sixteen-byte
  descriptors, tails written one past each ring as doorbells, written-back
  lengths checked against the buffer, a fatal bus error terminal, memory given
  back only after a reset seen to finish. 26 host tests against a model of the
  core that walks the rings by device address over scattered pages and lies
  when asked (a reset that never ends, a busy bus, no PHY, lengths past the
  buffer, error summaries, context descriptors, split frames, a bus error).
* `src/lib/drivers/net/yt6801` (`ferrix-yt6801`): the chip around the core --
  its reset, the PHY's release, the address from the eFuse's patch region
  (else what firmware left in the core, else a locally administered
  `02:1f:0a:…`), interrupts let out, and the core's options for one vector.
  10 host tests against a model of BAR 0 that refuses any touch of the MSI-X
  table.
* `src/user/system/native/drivers/net/yt6801` (`yt6801`): the process, the
  virtio-net program with this device under it, over `ferrix-netserve`; it
  polls every millisecond until it has seen an interrupt and asks the PHY
  about the link every 250 ms, sending LINK on a change. `devmgr` starts it on
  `1F0A:6801`.
* The kernel: the chip's MSI-X table shares the page with those registers,
  and the kernel never grants a page holding a table it uses. For this one
  function it holds MSI-X off for good and grants BAR 0 whole, offering the
  MSI vector instead (`docs/DEVMGR.md` §3, `L.device.35`-`37`, the
  consultant's OK IF of 2026-10-15). Stage 10 checks that path on QEMU's
  `e1000e`, so every x86-64 boot exercises it.

**On the box (2026-10-15, `docs/N150.md` §13):** the link came up at 1 Gbit
and the launcher reached the internet. The firmware had left the function's
memory decoding off (enumeration now turns it on), and the MSI vector never
delivers, so the driver runs on its polling fallback; why MSI never arrives
on the box is not known.

**Proven** on the host models and, for the kernel's path, on QEMU's `e1000e`.
**Not proven:** anything about the chip itself. QEMU models no dwmac4 core at
all (9.2 and 10.2 have only the NPCM GMAC and the XGMAC), so the driver has
never met real hardware. The first boot on the box must print, and the
session reading it must check:

1. The devices line and `yt6801   0x… 1f0a:6801 apertures … vectors 1
   msix_table_size 0`, and the aperture lines: one aperture of BAR 0 from
   offset 0 covering at least `0x4000` (the glue's page granted).
2. That MSI-X control reads Enable 0 (`device_config_read` of the MSI-X
   capability) and that the one MSI vector's deliveries count under traffic.
3. The MSI capability and MSI-X control read before and after the chip's
   `SYS_RESET` -- if the reset clears the MSI capability the node is refused at
   the next HELLO, which fails closed but ends the NIC until reboot.
4. `yt6801   address … from Efuse` (the eFuse's address; `Firmware` or
   `Generated` means the eFuse read nothing -- read the `eFuse` field).
5. `yt6801   core 0x…` with no "did not come up" line: the core's software
   reset finished (`ResetTimeout` means the PHY gave the core no clock).
6. The PHY identifier: `phy 0x4f51e91a` expected for the YT8531S.
7. `yt6801   first interrupt` within a second of traffic: interrupts are
   delivered (without it the driver still works by polling, slowly).
8. `yt6801   link Up { speed: Thousand, full_duplex: true }` with a cable to
   a gigabit switch, and then `ping` and `wget` as `test-net` does.
9. Linux disables ASPM's L1 on this chip because some cards deliver no MSI
   with it on; Ferrix leaves link power management as firmware set it. If
   step 7 fails, that is the first suspect.

**Exit, and it is met:** under `xtask`'s gateway — which is where this
criterion's *"under QEMU's user-mode network"* now reads — busybox configures
`eth0` with `ip`, and `route` and `netstat` report through `/proc/net`. `wget`
fetches a file from a server on the host that byte-for-byte matches what it
served. All of it runs in a test of its own, `cargo xtask test-net`, for the
reason stage 7's exit is one, and it passes on all three architectures.

The criterion's `nc` clause is met by other programs, deliberately: this
busybox's `nc` has no `-U`, so it cannot open a local socket at all, and a
criterion written before that was known is not worth bending the code to. A
stream over `AF_UNIX` — bound to a path and to an abstract name, connected,
accepted, and carrying bytes each way — is proven by the stage 7 boot check on
every architecture, and by `su`, which reaches `/etc/group` only because a
`connect` to a name nobody bound answers the way a C library expects.

---

