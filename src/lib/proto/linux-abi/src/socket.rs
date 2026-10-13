//! Sockets: the numbers `socket`, `setsockopt` and `sendmsg` take, and the
//! layouts of the structures they pass by pointer.
//!
//! Ferrix's first sockets are `AF_UNIX` ones, because that is what the
//! programs it runs reach for before any network exists: a `socketpair` as a
//! two-way pipe, a daemon's control socket, a descriptor handed from one
//! process to another in an `SCM_RIGHTS` message. So this module is the part
//! of the socket ABI those need, written down once.
//!
//! # One table for three architectures
//!
//! Every option and flag number here is the one in
//! `include/uapi/asm-generic/socket.h`, `linux/socket.h` and `linux/un.h`, and
//! x86-64, AArch64 and ARMv7-A all take the generic socket header unchanged.
//! The architectures that renumber socket options -- alpha, MIPS, PA-RISC and
//! SPARC among them -- are not ones Ferrix runs on, so unlike [`crate::nr`]
//! there is nothing per architecture to fold.
//!
//! # Why two of the layouts are decoders rather than structures
//!
//! Most of [`crate::types`] is `#[repr(C)]` structures the kernel writes out
//! whole, because most of that ABI is the same size on every architecture.
//! `struct msghdr` and `struct cmsghdr` are not: both carry `size_t` fields,
//! so ARMv7-A's are narrower than the 64-bit ones, and no one Rust structure
//! has both layouts. They are given instead as a plain value and functions
//! over little-endian bytes that take the pointer [`Width`] as an argument --
//! a value rather than a `cfg`, so the host tests check the 32-bit layout on a
//! 64-bit machine. Little-endian because all three architectures run that way
//! under Ferrix. [`Ucred`] and [`Linger`] have no pointer-sized field and are
//! `repr(C)` like the rest of the crate.
//!
//! Every decoder answers `None` for a buffer too short to hold what it reads,
//! and every encoder `None` for a buffer too short or a value too wide for a
//! 32-bit word, rather than panicking or truncating: the bytes come from a
//! program, and a short one is that program's mistake to be told about.

use crate::types::{FIONREAD, O_CLOEXEC, O_NONBLOCK, TIOCOUTQ};

// ---------------------------------------------------------------------------
// Pointer width
// ---------------------------------------------------------------------------

/// The width of a user pointer, and so of a `size_t`, in the program whose
/// structure is being read.
///
/// `src/lib/kernel/ustack` has a type of the same name for the same fact. This crate
/// cannot use it: `ferrix-ustack` depends on this crate rather than the other
/// way round, and this one has no dependencies by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// 32-bit, which for Ferrix means ARMv7-A.
    Bits32,
    /// 64-bit: x86-64 and AArch64.
    Bits64,
}

impl Width {
    /// Bytes in one pointer or `size_t`.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Width::Bits32 => 4,
            Width::Bits64 => 8,
        }
    }

    /// The word at `at`: four bytes widened, or eight.
    #[must_use]
    pub fn word(self, bytes: &[u8], at: usize) -> Option<u64> {
        match self {
            Width::Bits32 => read_u32(bytes, at).map(u64::from),
            Width::Bits64 => {
                let field = bytes.get(at..at.checked_add(8)?)?;
                Some(u64::from_le_bytes(field.try_into().ok()?))
            }
        }
    }

    /// Write `value` as one word at `at`.
    ///
    /// `None`, with nothing written, if the buffer is too short or the value
    /// does not fit a 32-bit word: a length the kernel means to report must
    /// not arrive in the program cut down to its low half.
    pub fn put_word(self, bytes: &mut [u8], at: usize, value: u64) -> Option<()> {
        match self {
            Width::Bits32 => put(bytes, at, &u32::try_from(value).ok()?.to_le_bytes()),
            Width::Bits64 => put(bytes, at, &value.to_le_bytes()),
        }
    }
}

/// The 32-bit field at `at`.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(field.try_into().ok()?))
}

/// Write `field` at `at`, or nothing if it does not fit.
fn put(bytes: &mut [u8], at: usize, field: &[u8]) -> Option<()> {
    bytes
        .get_mut(at..at.checked_add(field.len())?)?
        .copy_from_slice(field);
    Some(())
}

// ---------------------------------------------------------------------------
// Address families
// ---------------------------------------------------------------------------
//
// `sa_family_t` is a 16-bit field at the start of every `struct sockaddr`, so
// the families are `u16`: that field is where the kernel compares them. The
// `PF_` names a C library also defines are the same numbers.

/// No family: what `connect` on a datagram socket is given to dissolve its
/// association.
pub const AF_UNSPEC: u16 = 0;
/// Local sockets, named by a path or an abstract name.
pub const AF_UNIX: u16 = 1;
/// The POSIX name for [`AF_UNIX`].
pub const AF_LOCAL: u16 = AF_UNIX;
/// IPv4.
pub const AF_INET: u16 = 2;
/// IPv6.
pub const AF_INET6: u16 = 10;
/// The kernel's own message bus, which `ip` and `udev` speak.
pub const AF_NETLINK: u16 = 16;
/// Frames on a link, below IP: what a DHCP client and `tcpdump` open.
pub const AF_PACKET: u16 = 17;
/// One past the highest family the kernel knows.
///
/// Linux's own `AF_MAX` is 46, since `AF_MCTP` took 45. musl 1.2.5 still has
/// `PF_MAX` 45 from before that; the kernel's number is the one a family is
/// refused against.
pub const AF_MAX: u16 = 46;

// ---------------------------------------------------------------------------
// Socket types
// ---------------------------------------------------------------------------

/// A connected, reliable byte stream with no message boundaries.
pub const SOCK_STREAM: u32 = 1;
/// Datagrams: each send one message, delivered whole or not at all.
pub const SOCK_DGRAM: u32 = 2;
/// Raw network protocol access.
pub const SOCK_RAW: u32 = 3;
/// Reliably delivered messages. No Linux family implements it but RDS.
pub const SOCK_RDM: u32 = 4;
/// A connected, reliable stream of records: [`SOCK_STREAM`]'s delivery with
/// [`SOCK_DGRAM`]'s boundaries.
pub const SOCK_SEQPACKET: u32 = 5;
/// The Datagram Congestion Control Protocol.
pub const SOCK_DCCP: u32 = 6;
/// The obsolete packet interface, before `AF_PACKET`.
pub const SOCK_PACKET: u32 = 10;
/// The bits of `socket`'s type argument that are the type; the rest are
/// [`SOCK_NONBLOCK`] and [`SOCK_CLOEXEC`].
pub const SOCK_TYPE_MASK: u32 = 0xf;
/// Create the socket non-blocking, as if `fcntl` had set `O_NONBLOCK`.
///
/// Linux defines it *as* `O_NONBLOCK`, so it is `0o4000` exactly where that
/// is, which is all three architectures here.
pub const SOCK_NONBLOCK: u32 = O_NONBLOCK;
/// Create the descriptor close-on-exec. Defined as `O_CLOEXEC`, likewise.
pub const SOCK_CLOEXEC: u32 = O_CLOEXEC;

// ---------------------------------------------------------------------------
// Message flags
// ---------------------------------------------------------------------------

/// Send or receive out-of-band data.
pub const MSG_OOB: u32 = 0x1;
/// Receive without taking the data off the queue.
pub const MSG_PEEK: u32 = 0x2;
/// Send only to directly connected hosts.
pub const MSG_DONTROUTE: u32 = 0x4;
/// `msg_flags` on return: the control data was cut short for want of room.
pub const MSG_CTRUNC: u32 = 0x8;
/// Supply or ask for a transport-layer proxy address.
pub const MSG_PROXY: u32 = 0x10;
/// `recv`: return a datagram's real length even if it was cut short.
/// `msg_flags` on return: it was.
pub const MSG_TRUNC: u32 = 0x20;
/// This one call does not block, whatever the descriptor's `O_NONBLOCK`.
pub const MSG_DONTWAIT: u32 = 0x40;
/// End of record, for types that have records.
pub const MSG_EOR: u32 = 0x80;
/// Wait until the whole request is satisfied.
pub const MSG_WAITALL: u32 = 0x100;
/// TCP's FIN, as a flag.
pub const MSG_FIN: u32 = 0x200;
/// TCP's SYN, as a flag.
pub const MSG_SYN: u32 = 0x400;
/// Confirm the path is still valid, so neighbour discovery need not ask.
pub const MSG_CONFIRM: u32 = 0x800;
/// TCP's RST, as a flag.
pub const MSG_RST: u32 = 0x1000;
/// Receive from the error queue rather than the data queue.
pub const MSG_ERRQUEUE: u32 = 0x2000;
/// Report `EPIPE` for a broken stream without raising `SIGPIPE`.
pub const MSG_NOSIGNAL: u32 = 0x4000;
/// More data follows: hold this send back to coalesce it.
pub const MSG_MORE: u32 = 0x8000;
/// `recvmmsg`: block for the first message only.
pub const MSG_WAITFORONE: u32 = 0x10000;
/// `sendmmsg`: more messages follow in this batch.
pub const MSG_BATCH: u32 = 0x40000;
/// Send without copying, reporting completion on the error queue.
pub const MSG_ZEROCOPY: u32 = 0x400_0000;
/// Send data in TCP's SYN.
pub const MSG_FASTOPEN: u32 = 0x2000_0000;
/// Set close-on-exec on descriptors received in `SCM_RIGHTS`.
pub const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;
/// Internal to the kernel's 32-bit compatibility path; a program never
/// passes it.
pub const MSG_CMSG_COMPAT: u32 = 0x8000_0000;

// ---------------------------------------------------------------------------
// shutdown
// ---------------------------------------------------------------------------

/// `shutdown`: no more receiving.
pub const SHUT_RD: u32 = 0;
/// `shutdown`: no more sending; the peer reads end of file once drained.
pub const SHUT_WR: u32 = 1;
/// `shutdown`: both.
pub const SHUT_RDWR: u32 = 2;

// ---------------------------------------------------------------------------
// Socket options and control messages
// ---------------------------------------------------------------------------
//
// `i32` because `setsockopt`'s level and name are C `int`s, and so are the
// `cmsg_level` and `cmsg_type` of a control message, which is where the kernel
// compares [`SOL_SOCKET`] and the `SCM_` numbers against what a program wrote.

/// The level of options that belong to the socket rather than a protocol.
pub const SOL_SOCKET: i32 = 1;

// ---------------------------------------------------------------------------
// AF_PACKET
// ---------------------------------------------------------------------------

/// Options of packet sockets.
pub const SOL_PACKET: i32 = 263;
/// `struct sockaddr_ll`: family, protocol, interface index, hardware type,
/// packet type, address length and an eight-byte address.
pub const SOCKADDR_LL_SIZE: usize = 20;
/// Where `sll_addr` starts in a `sockaddr_ll`, which is how long one naming
/// no address is.
pub const SOCKADDR_LL_ADDR_OFFSET: usize = 12;

/// Record debugging information, for protocols that keep any.
pub const SO_DEBUG: i32 = 1;
/// Allow binding an address still in `TIME_WAIT`.
pub const SO_REUSEADDR: i32 = 2;
/// Read only: the socket's type, a `SOCK_` number.
pub const SO_TYPE: i32 = 3;
/// Read only: the pending error, cleared by reading it.
pub const SO_ERROR: i32 = 4;
/// Send only to directly connected hosts, as [`MSG_DONTROUTE`] on every send.
pub const SO_DONTROUTE: i32 = 5;
/// Allow sending to a broadcast address.
pub const SO_BROADCAST: i32 = 6;
/// The send buffer's size in bytes. Linux doubles what is set, to allow for
/// its bookkeeping, and reports the doubled value.
pub const SO_SNDBUF: i32 = 7;
/// The receive buffer's size in bytes, doubled the same way.
pub const SO_RCVBUF: i32 = 8;
/// Send keep-alive probes on an idle connection.
pub const SO_KEEPALIVE: i32 = 9;
/// Deliver out-of-band data in the ordinary stream.
pub const SO_OOBINLINE: i32 = 10;
/// Skip UDP checksums.
pub const SO_NO_CHECK: i32 = 11;
/// The queueing priority of sent packets.
pub const SO_PRIORITY: i32 = 12;
/// How `close` waits for unsent data: a [`Linger`].
pub const SO_LINGER: i32 = 13;
/// Accepted and ignored since Linux 2.2.
pub const SO_BSDCOMPAT: i32 = 14;
/// Let several sockets bind one address and share its traffic.
pub const SO_REUSEPORT: i32 = 15;
/// Receive the sender's credentials as `SCM_CREDENTIALS` with every message.
pub const SO_PASSCRED: i32 = 16;
/// Read only: the credentials of the connected peer, as a [`Ucred`] taken
/// when the connection was made.
pub const SO_PEERCRED: i32 = 17;
/// The fewest bytes a receive waits for.
pub const SO_RCVLOWAT: i32 = 18;
/// The fewest bytes a send waits for room for. Linux refuses to set it.
pub const SO_SNDLOWAT: i32 = 19;
/// The receive timeout, as a `struct timeval` of two words.
///
/// A 64-bit program's `SO_RCVTIMEO` is this number, whose words are already
/// 64-bit. A 32-bit musl's `SO_RCVTIMEO` is [`SO_RCVTIMEO_NEW`], with a
/// 64-bit `time_t`, and it falls back to this one with 32-bit words only when
/// the new number is refused with `ENOPROTOOPT`.
pub const SO_RCVTIMEO_OLD: i32 = 20;
/// The send timeout, with [`SO_RCVTIMEO_OLD`]'s layout.
pub const SO_SNDTIMEO_OLD: i32 = 21;
/// Read only: whether `listen` has been called.
pub const SO_ACCEPTCONN: i32 = 30;
/// Read only: the peer's security context.
pub const SO_PEERSEC: i32 = 31;
/// [`SO_SNDBUF`] past `net.core.wmem_max`, for a privileged caller.
pub const SO_SNDBUFFORCE: i32 = 32;
/// [`SO_RCVBUF`] past `net.core.rmem_max`, for a privileged caller.
pub const SO_RCVBUFFORCE: i32 = 33;
/// Receive the sender's security context with every message.
pub const SO_PASSSEC: i32 = 34;
/// Read only: the socket's protocol number.
pub const SO_PROTOCOL: i32 = 38;
/// Read only: the socket's family, an `AF_` number.
pub const SO_DOMAIN: i32 = 39;
/// The offset a [`MSG_PEEK`] receive starts at, or -1 for none.
pub const SO_PEEK_OFF: i32 = 42;
/// Read only: the connected peer's supplementary groups.
pub const SO_PEERGROUPS: i32 = 59;
/// The receive timeout as a `struct __kernel_sock_timeval`: two 64-bit
/// fields on every architecture.
pub const SO_RCVTIMEO_NEW: i32 = 66;
/// The send timeout, with [`SO_RCVTIMEO_NEW`]'s layout.
pub const SO_SNDTIMEO_NEW: i32 = 67;
/// Receive the sender's process as a pidfd with every message.
pub const SO_PASSPIDFD: i32 = 76;
/// Read only: a pidfd for the connected peer.
pub const SO_PEERPIDFD: i32 = 77;

/// A control message carrying open descriptors, as an array of `int`.
pub const SCM_RIGHTS: i32 = 1;
/// A control message carrying credentials, as a [`Ucred`].
pub const SCM_CREDENTIALS: i32 = 2;
/// The most descriptors one `SCM_RIGHTS` message may carry. More is
/// `EINVAL` from `sendmsg`.
pub const SCM_MAX_FD: usize = 253;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Bytes in `sun_path`, the name field of [`struct sockaddr_un`](UnixAddress).
pub const UNIX_PATH_MAX: usize = 108;

/// The longest `listen` backlog honoured: Linux's default
/// `net.core.somaxconn`, 4096 since Linux 5.4.
///
/// musl's header still says 128, the value before that. A program passes the
/// header's constant or its own number, and the kernel clamps either to this.
pub const SOMAXCONN: usize = 4096;

/// A new socket's send and receive buffer size: Linux's default
/// `net.core.rmem_default` and `wmem_default`.
pub const SOCKET_BUFFER_DEFAULT: usize = 212_992;

/// The smallest buffer `SO_SNDBUF` or `SO_RCVBUF` can make.
///
/// A Ferrix floor of one page, not a Linux number: Linux's `SOCK_MIN_RCVBUF`
/// is about 2.3 KiB, derived from its buffer bookkeeping, which Ferrix does
/// not have. One page means any write of a page or less can eventually be
/// accepted whole.
pub const SOCKET_BUFFER_MIN: usize = 4096;

/// The largest buffer `SO_SNDBUF` or `SO_RCVBUF` can make without the
/// `FORCE` options: Linux's default `net.core.rmem_max`, which is the same
/// number as the default size.
pub const SOCKET_BUFFER_MAX: usize = SOCKET_BUFFER_DEFAULT;

// ---------------------------------------------------------------------------
// ioctls
// ---------------------------------------------------------------------------

/// How many bytes a receive would return: the terminal request [`FIONREAD`]
/// under its socket name, from `linux/sockios.h`.
pub const SIOCINQ: u32 = FIONREAD;
/// How many bytes are queued unsent: the terminal request [`TIOCOUTQ`] under
/// its socket name.
pub const SIOCOUTQ: u32 = TIOCOUTQ;

/// The interface ioctls, from `linux/sockios.h`. Every one takes a pointer to
/// a `struct ifreq` -- `SIOCGIFCONF` a `struct ifconf` -- and every one is
/// what a program that was not written against netlink uses: `ifconfig`,
/// `if_nametoindex`, `getifaddrs`, and busybox's `ip link set`, which asks for
/// the flags this way even though it sets everything else over rtnetlink.
///
/// The index a name has.
pub const SIOCGIFNAME: u32 = 0x8910;
/// Every interface with an address, as an array of `struct ifreq`.
pub const SIOCGIFCONF: u32 = 0x8912;
/// The `IFF_` flags.
pub const SIOCGIFFLAGS: u32 = 0x8913;
/// Set them.
pub const SIOCSIFFLAGS: u32 = 0x8914;
/// The interface's first address.
pub const SIOCGIFADDR: u32 = 0x8915;
/// Set it.
pub const SIOCSIFADDR: u32 = 0x8916;
/// The address at the far end of a point-to-point link.
pub const SIOCGIFDSTADDR: u32 = 0x8917;
/// Set it.
pub const SIOCSIFDSTADDR: u32 = 0x8918;
/// The broadcast address.
pub const SIOCGIFBRDADDR: u32 = 0x8919;
/// Set it.
pub const SIOCSIFBRDADDR: u32 = 0x891A;
/// The network mask.
pub const SIOCGIFNETMASK: u32 = 0x891B;
/// Set it.
pub const SIOCSIFNETMASK: u32 = 0x891C;
/// The route metric, which Linux always reports as zero.
pub const SIOCGIFMETRIC: u32 = 0x891D;
/// Set it, which Linux always refuses.
pub const SIOCSIFMETRIC: u32 = 0x891E;
/// The MTU.
pub const SIOCGIFMTU: u32 = 0x8921;
/// Set it.
pub const SIOCSIFMTU: u32 = 0x8922;
/// The hardware address, as a `sockaddr` whose family is the `ARPHRD_` kind.
pub const SIOCGIFHWADDR: u32 = 0x8927;
/// The index of the interface a name has.
pub const SIOCGIFINDEX: u32 = 0x8933;
/// Take an address away.
pub const SIOCDIFADDR: u32 = 0x8936;
/// How many interfaces there are.
pub const SIOCGIFCOUNT: u32 = 0x8938;
/// The transmit queue's length.
pub const SIOCGIFTXQLEN: u32 = 0x8942;
/// Set it.
pub const SIOCSIFTXQLEN: u32 = 0x8943;

/// The bytes of `ifr_name`, which is `IFNAMSIZ`: fifteen characters and a
/// terminator.
pub const IFNAMSIZ: usize = 16;

/// Where `ifr_ifru`, the union after the name, starts.
pub const IFREQ_UNION: usize = IFNAMSIZ;

/// The bytes of a `struct ifreq`.
///
/// The union after the name is a `struct sockaddr` (16 bytes), an `int`, a
/// `short`, a pointer, or a `struct ifmap`, and `ifmap` is the longest: two
/// `unsigned long`s and five bytes, which pads to 24 where a long is eight
/// bytes and to 16 where it is four. Nothing here reads a field beyond the
/// first sixteen bytes of the union, so only `SIOCGIFCONF`, which walks an
/// array of these, depends on the size at all.
pub const IFREQ_BYTES: usize = IFNAMSIZ + if size_of::<usize>() == 8 { 24 } else { 16 };

// ---------------------------------------------------------------------------
// struct msghdr
// ---------------------------------------------------------------------------

/// `struct msghdr`, as the kernel reads it from `sendmsg` and `recvmsg`.
///
/// Every field starts on a word boundary: the pointers and the two `size_t`
/// lengths are a word each, and the two 32-bit fields, `msg_namelen` and
/// `msg_flags`, are followed by padding to one on a 64-bit target. So the
/// structure is seven words, 56 bytes or 28, and the *n*th field is at *n*
/// words.
///
/// The kernel's `msg_iovlen` and `msg_controllen` are `size_t`. musl's 64-bit
/// header declares them `int` and `socklen_t` with a padding field beside
/// each, as POSIX asks, and zeroes that padding in `sendmsg` and `recvmsg`
/// before the call -- so reading the whole word, as [`MsgHdr::decode`] does,
/// reads the same number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MsgHdr {
    /// `msg_name`: the user address of a socket address, or zero.
    pub name: u64,
    /// `msg_namelen`: bytes at [`MsgHdr::name`]; on return, the address's
    /// full length.
    pub name_len: u32,
    /// `msg_iov`: the user address of an array of `struct iovec`.
    pub iov: u64,
    /// `msg_iovlen`: entries in that array.
    pub iov_len: u64,
    /// `msg_control`: the user address of the control message buffer.
    pub control: u64,
    /// `msg_controllen`: bytes in it; on return, the bytes used.
    pub control_len: u64,
    /// `msg_flags`: ignored on send; on return, `MSG_TRUNC`, `MSG_CTRUNC`
    /// and `MSG_EOR`.
    pub flags: u32,
}

impl MsgHdr {
    /// Bytes in the structure: seven words.
    #[must_use]
    pub const fn size(width: Width) -> usize {
        7 * width.bytes()
    }

    /// Where `msg_namelen` is, for the kernel to write back an address's
    /// length alone.
    #[must_use]
    pub const fn name_len_offset(width: Width) -> usize {
        width.bytes()
    }

    /// Where `msg_iov` is.
    #[must_use]
    pub const fn iov_offset(width: Width) -> usize {
        2 * width.bytes()
    }

    /// Where `msg_iovlen` is.
    #[must_use]
    pub const fn iov_len_offset(width: Width) -> usize {
        3 * width.bytes()
    }

    /// Where `msg_control` is.
    #[must_use]
    pub const fn control_offset(width: Width) -> usize {
        4 * width.bytes()
    }

    /// Where `msg_controllen` is, for the kernel to write back the control
    /// bytes it used. A word: write it with [`Width::put_word`].
    #[must_use]
    pub const fn control_len_offset(width: Width) -> usize {
        5 * width.bytes()
    }

    /// Where `msg_flags` is, for the kernel to write back what happened.
    #[must_use]
    pub const fn flags_offset(width: Width) -> usize {
        6 * width.bytes()
    }

    /// Read a `struct msghdr` from the start of `bytes`.
    ///
    /// `bytes` must hold the whole structure, trailing padding included,
    /// because the kernel copies the whole structure in: a buffer that ends
    /// inside it is one the program did not wholly map.
    #[must_use]
    pub fn decode(bytes: &[u8], width: Width) -> Option<MsgHdr> {
        if bytes.len() < Self::size(width) {
            return None;
        }
        Some(MsgHdr {
            name: width.word(bytes, 0)?,
            name_len: read_u32(bytes, Self::name_len_offset(width))?,
            iov: width.word(bytes, Self::iov_offset(width))?,
            iov_len: width.word(bytes, Self::iov_len_offset(width))?,
            control: width.word(bytes, Self::control_offset(width))?,
            control_len: width.word(bytes, Self::control_len_offset(width))?,
            flags: read_u32(bytes, Self::flags_offset(width))?,
        })
    }

    /// Write the whole structure over the first [`MsgHdr::size`] bytes of
    /// `bytes`, padding zero.
    ///
    /// Built in a local buffer and copied only once every field has fitted,
    /// so a value too wide for a 32-bit word leaves `bytes` untouched.
    pub fn encode(&self, bytes: &mut [u8], width: Width) -> Option<()> {
        let size = Self::size(width);
        let mut record = [0_u8; 56];
        width.put_word(&mut record, 0, self.name)?;
        put(
            &mut record,
            Self::name_len_offset(width),
            &self.name_len.to_le_bytes(),
        )?;
        width.put_word(&mut record, Self::iov_offset(width), self.iov)?;
        width.put_word(&mut record, Self::iov_len_offset(width), self.iov_len)?;
        width.put_word(&mut record, Self::control_offset(width), self.control)?;
        width.put_word(
            &mut record,
            Self::control_len_offset(width),
            self.control_len,
        )?;
        put(
            &mut record,
            Self::flags_offset(width),
            &self.flags.to_le_bytes(),
        )?;
        bytes.get_mut(..size)?.copy_from_slice(record.get(..size)?);
        Some(())
    }
}

/// `struct mmsghdr`, the element of the array `sendmmsg` and `recvmmsg`
/// take: a [`MsgHdr`] and then `unsigned int msg_len`, the bytes that message
/// sent or received, padded to the structure's word alignment. 64 bytes on a
/// 64-bit target and 32 on a 32-bit one -- `compat_mmsghdr` for an i386
/// program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmsgHdr;

impl MmsgHdr {
    /// Where `msg_len` is: just past the `msghdr`.
    #[must_use]
    pub const fn len_offset(width: Width) -> usize {
        MsgHdr::size(width)
    }

    /// Bytes in one element, and so the distance from one to the next.
    #[must_use]
    pub const fn size(width: Width) -> usize {
        (MsgHdr::size(width) + 4).next_multiple_of(width.bytes())
    }
}

// ---------------------------------------------------------------------------
// struct cmsghdr
// ---------------------------------------------------------------------------

/// The header of one control message, `struct cmsghdr`.
///
/// `cmsg_len` is a `size_t` and counts the header too; `cmsg_level` and
/// `cmsg_type` are `int`s after it. So 16 bytes on a 64-bit target and 12 on
/// ARMv7-A, both already multiples of the word, which is why the aligned
/// header size [`cmsg_len`] adds is the header size itself. musl's 64-bit
/// header makes `cmsg_len` a `socklen_t` and a padding field, and `sendmsg`
/// zeroes the padding in its copy of the buffer, so the word reads the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CmsgHdr {
    /// `cmsg_len`: bytes in the header and the data, but not the padding
    /// after the data.
    pub len: u64,
    /// `cmsg_level`: [`SOL_SOCKET`] for the messages here.
    pub level: i32,
    /// `cmsg_type`, renamed because `type` is a keyword: [`SCM_RIGHTS`] or
    /// [`SCM_CREDENTIALS`].
    pub kind: i32,
}

impl CmsgHdr {
    /// Bytes in the header: a word and two `int`s.
    #[must_use]
    pub const fn size(width: Width) -> usize {
        width.bytes() + 8
    }

    /// Where `cmsg_level` is.
    #[must_use]
    pub const fn level_offset(width: Width) -> usize {
        width.bytes()
    }

    /// Where `cmsg_type` is.
    #[must_use]
    pub const fn kind_offset(width: Width) -> usize {
        width.bytes() + 4
    }

    /// Read a header from the start of `bytes`.
    #[must_use]
    pub fn decode(bytes: &[u8], width: Width) -> Option<CmsgHdr> {
        Some(CmsgHdr {
            len: width.word(bytes, 0)?,
            level: i32::from_le_bytes(read_u32(bytes, Self::level_offset(width))?.to_le_bytes()),
            kind: i32::from_le_bytes(read_u32(bytes, Self::kind_offset(width))?.to_le_bytes()),
        })
    }

    /// Write the header over the first [`CmsgHdr::size`] bytes of `bytes`,
    /// leaving them untouched if it does not fit.
    pub fn encode(&self, bytes: &mut [u8], width: Width) -> Option<()> {
        let size = Self::size(width);
        let mut header = [0_u8; 16];
        width.put_word(&mut header, 0, self.len)?;
        put(
            &mut header,
            Self::level_offset(width),
            &self.level.to_le_bytes(),
        )?;
        put(
            &mut header,
            Self::kind_offset(width),
            &self.kind.to_le_bytes(),
        )?;
        bytes.get_mut(..size)?.copy_from_slice(header.get(..size)?);
        Some(())
    }
}

/// `CMSG_ALIGN`: `len` rounded up to a multiple of the word.
///
/// The C macro rounds to `sizeof(size_t)`, so control messages pack
/// differently on ARMv7-A: four `int`s of `SCM_RIGHTS` data need no padding
/// there and none on a 64-bit target either, but one `int` needs four bytes of
/// it on 64-bit only. It saturates rather than wrapping, so a length within a
/// word of `usize::MAX`, which no real buffer has, is never made small.
#[must_use]
pub const fn cmsg_align(len: usize, width: Width) -> usize {
    let word = width.bytes();
    len.div_ceil(word).saturating_mul(word)
}

/// `CMSG_SPACE`: the bytes a message with `data_len` bytes of data takes in
/// the buffer, the padding after its data included. What a program sizes
/// `msg_control` with.
#[must_use]
pub const fn cmsg_space(data_len: usize, width: Width) -> usize {
    cmsg_align(data_len, width).saturating_add(cmsg_align(CmsgHdr::size(width), width))
}

/// `CMSG_LEN`: what `cmsg_len` says for `data_len` bytes of data -- the
/// header and the data, but not the padding after it.
#[must_use]
pub const fn cmsg_len(data_len: usize, width: Width) -> usize {
    cmsg_align(CmsgHdr::size(width), width).saturating_add(data_len)
}

/// One control message found by [`ControlMessages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlMessage<'a> {
    /// `cmsg_level`.
    pub level: i32,
    /// `cmsg_type`.
    pub kind: i32,
    /// The data: `cmsg_len` less the header, never the padding.
    pub data: &'a [u8],
}

/// A control message whose `cmsg_len` is shorter than its own header or runs
/// past the end of the buffer: Linux's `CMSG_OK` failing, which `sendmsg`
/// answers with `EINVAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadControlMessage;

/// The control messages in a `msg_control` buffer, walked as the kernel's
/// `for_each_cmsghdr` walks them.
///
/// There is a first message if the buffer holds a whole header, and a next
/// one if a whole header fits after the current message's aligned length.
/// Bytes after the last message too few to be a header are ignored, as Linux
/// ignores them. A message that fails `CMSG_OK` is yielded as an error and
/// ends the walk, because its length is the only way to find the next one.
#[derive(Debug, Clone)]
pub struct ControlMessages<'a> {
    control: &'a [u8],
    offset: usize,
    width: Width,
    done: bool,
}

impl<'a> ControlMessages<'a> {
    /// Walk `control`, the `msg_controllen` bytes a program passed.
    #[must_use]
    pub fn new(control: &'a [u8], width: Width) -> ControlMessages<'a> {
        ControlMessages {
            control,
            offset: 0,
            width,
            done: false,
        }
    }
}

impl<'a> Iterator for ControlMessages<'a> {
    type Item = Result<ControlMessage<'a>, BadControlMessage>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let header = CmsgHdr::size(self.width);
        let rest = self.control.get(self.offset..).unwrap_or_default();
        let Some(found) = CmsgHdr::decode(rest, self.width) else {
            self.done = true;
            return None;
        };
        let data = usize::try_from(found.len)
            .ok()
            .filter(|&len| len >= header)
            .and_then(|len| rest.get(header..len));
        let Some(data) = data else {
            self.done = true;
            return Some(Err(BadControlMessage));
        };
        let len = header.saturating_add(data.len());
        self.offset = self
            .offset
            .saturating_add(cmsg_align(len, self.width))
            .min(self.control.len());
        Some(Ok(ControlMessage {
            level: found.level,
            kind: found.kind,
            data,
        }))
    }
}

// ---------------------------------------------------------------------------
// struct sockaddr_un
// ---------------------------------------------------------------------------

/// Where `sun_path` starts: after the 16-bit `sun_family`.
pub const SUN_PATH_OFFSET: usize = 2;

/// Bytes in `struct sockaddr_un`: the family and [`UNIX_PATH_MAX`] of path.
pub const SOCKADDR_UN_SIZE: usize = SUN_PATH_OFFSET + UNIX_PATH_MAX;

/// What an `AF_UNIX` address names, read from a `struct sockaddr_un` and the
/// length a program gave with it.
///
/// Linux gives the length a meaning of its own rather than trusting a
/// terminator, and the three cases are told apart by it and the first byte of
/// the path:
///
/// * Just the family is an unnamed address: what `bind` autobinds and what
///   `getsockname` reports for a socket that was never bound.
/// * A path starting with a NUL is an abstract name: everything after that
///   NUL up to the length, NULs included, with nothing on any filesystem.
/// * Anything else is a filesystem path, which ends at the first NUL within
///   the length, or at the length if there is none -- so a path of the full
///   108 bytes needs no terminator.
///
/// Which calls accept which is the kernel's business: `bind` takes all
/// three, while `connect` refuses an unnamed address with `EINVAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixAddress<'a> {
    /// The family alone.
    Unnamed,
    /// A filesystem path, without its terminator. Never empty.
    Path(&'a [u8]),
    /// An abstract name, without the leading NUL. May be empty, and may
    /// contain NULs.
    Abstract(&'a [u8]),
}

/// Why an address is not an `AF_UNIX` one. Linux answers `EINVAL` to each;
/// they are told apart for the kernel's logs and the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressError {
    /// The length does not cover the family, or `bytes` is shorter than the
    /// length claims.
    TooShort,
    /// The length is more than a `struct sockaddr_un`.
    TooLong,
    /// `sun_family` is not [`AF_UNIX`].
    WrongFamily,
}

impl<'a> UnixAddress<'a> {
    /// Read the address in the first `addr_len` bytes of `bytes`.
    pub fn parse(bytes: &'a [u8], addr_len: usize) -> Result<UnixAddress<'a>, AddressError> {
        if addr_len > SOCKADDR_UN_SIZE {
            return Err(AddressError::TooLong);
        }
        let Some([low, high, path @ ..]) = bytes.get(..addr_len) else {
            return Err(AddressError::TooShort);
        };
        if u16::from_le_bytes([*low, *high]) != AF_UNIX {
            return Err(AddressError::WrongFamily);
        }
        Ok(match path {
            [] => UnixAddress::Unnamed,
            [0, name @ ..] => UnixAddress::Abstract(name),
            _ => UnixAddress::Path(path.split(|&byte| byte == 0).next().unwrap_or(path)),
        })
    }

    /// The length [`UnixAddress::encode`] reports: the family, the name, and
    /// a path's terminator if there is room for one.
    ///
    /// Linux reports a path's length as its `strlen` plus three whether or
    /// not the terminator fits, so a 108-byte path comes back as 111, longer
    /// than the structure. Ferrix says 110, the bytes that exist; a program
    /// comparing the answer against `sizeof(struct sockaddr_un)` to detect
    /// truncation is then not told of a truncation that did not happen.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            UnixAddress::Unnamed => SUN_PATH_OFFSET,
            UnixAddress::Path(path) => SUN_PATH_OFFSET
                .saturating_add(path.len())
                .saturating_add(1)
                .min(SOCKADDR_UN_SIZE),
            UnixAddress::Abstract(name) => {
                SUN_PATH_OFFSET.saturating_add(1).saturating_add(name.len())
            }
        }
    }

    /// Write the address at the start of `out` and answer its length, as
    /// `getsockname`, `getpeername`, `accept` and `recvfrom` report one.
    ///
    /// `None`, with nothing written, if `out` is too short or the address is
    /// not one [`UnixAddress::parse`] could have produced: a path that is
    /// empty, holds a NUL or is longer than [`UNIX_PATH_MAX`], or an
    /// abstract name longer than the path field less its leading NUL.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let valid = match self {
            UnixAddress::Unnamed => true,
            UnixAddress::Path(path) => {
                !path.is_empty() && !path.contains(&0) && path.len() <= UNIX_PATH_MAX
            }
            UnixAddress::Abstract(name) => name.len() < UNIX_PATH_MAX,
        };
        let len = self.encoded_len();
        if !valid || out.len() < len {
            return None;
        }
        put(out, 0, &AF_UNIX.to_le_bytes())?;
        match self {
            UnixAddress::Unnamed => {}
            UnixAddress::Path(path) => {
                put(out, SUN_PATH_OFFSET, path)?;
                if path.len() < UNIX_PATH_MAX {
                    put(out, SUN_PATH_OFFSET.saturating_add(path.len()), &[0])?;
                }
            }
            UnixAddress::Abstract(name) => {
                put(out, SUN_PATH_OFFSET, &[0])?;
                put(out, SUN_PATH_OFFSET.saturating_add(1), name)?;
            }
        }
        Some(len)
    }
}

// ---------------------------------------------------------------------------
// Credentials and linger
// ---------------------------------------------------------------------------

/// `struct ucred`: a process's identity as `SO_PEERCRED` and
/// `SCM_CREDENTIALS` carry it.
///
/// Three 32-bit fields, 12 bytes with no padding, on every architecture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Ucred {
    /// The process identifier, as the receiver's PID namespace sees it.
    pub pid: i32,
    /// The user identifier.
    pub uid: u32,
    /// The group identifier.
    pub gid: u32,
}

impl Ucred {
    /// Bytes in the structure.
    pub const SIZE: usize = 12;

    /// The structure's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0_u8; Self::SIZE];
        let fields = [
            self.pid.to_le_bytes(),
            self.uid.to_le_bytes(),
            self.gid.to_le_bytes(),
        ];
        for (slot, field) in bytes.chunks_exact_mut(4).zip(fields) {
            slot.copy_from_slice(&field);
        }
        bytes
    }

    /// Read the structure from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Ucred> {
        Some(Ucred {
            pid: i32::from_le_bytes(read_u32(bytes, 0)?.to_le_bytes()),
            uid: read_u32(bytes, 4)?,
            gid: read_u32(bytes, 8)?,
        })
    }
}

/// `struct linger`, the value of [`SO_LINGER`]: whether `close` waits for
/// unsent data, and for how many seconds.
///
/// Two `int`s on every architecture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Linger {
    /// `l_onoff`: non-zero to wait.
    pub onoff: i32,
    /// `l_linger`: the longest wait, in seconds.
    pub linger: i32,
}

impl Linger {
    /// Bytes in the structure.
    pub const SIZE: usize = 8;

    /// The structure's bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0_u8; Self::SIZE];
        let fields = [self.onoff.to_le_bytes(), self.linger.to_le_bytes()];
        for (slot, field) in bytes.chunks_exact_mut(4).zip(fields) {
            slot.copy_from_slice(&field);
        }
        bytes
    }

    /// Read the structure from the start of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Linger> {
        Some(Linger {
            onoff: i32::from_le_bytes(read_u32(bytes, 0)?.to_le_bytes()),
            linger: i32::from_le_bytes(read_u32(bytes, 4)?.to_le_bytes()),
        })
    }
}
