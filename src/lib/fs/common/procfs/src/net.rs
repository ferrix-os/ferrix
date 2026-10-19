//! The text of `/proc/net`.
//!
//! `route`, `netstat`, `ifconfig` and `ss` read these files with `sscanf` and
//! fixed columns written against what Linux prints. A field one column off is
//! a program that reads the wrong number and says so confidently, so every
//! format here is pinned in the tests against a line taken from a running
//! Linux.
//!
//! # The addresses are hexadecimal in host order, which looks backwards
//!
//! `/proc/net/route` prints `10.0.0.0` as `0000000A` and `/proc/net/tcp`
//! prints `127.0.0.1` as `0100007F`. That is not a mistake and not
//! big-endianness: the kernel holds the address in network order and prints
//! the `u32` those bytes make *in host order*, so on a little-endian machine
//! the octets come out reversed. Every reader of these files does the same
//! thing in reverse, so the format is what it is. [`hex_v4`] is the one place
//! it happens.
//!
//! # Lines are padded to a fixed width
//!
//! `/proc/net/route` and `udp` pad every line to 127 characters and `tcp` to
//! 149, so a reader can seek by record. The padding is Linux's `seq_pad`,
//! which pads a *short* line and leaves a long one alone -- so a row that
//! overflows its width simply overflows it, as an IPv6 row does.
//! `/proc/net/dev` and `/proc/net/arp` are not padded at all. All of it is
//! copied exactly.

use alloc::vec::Vec;

use crate::text::put;

/// How wide a line of `route` or `udp` is, before its newline.
pub const NARROW: usize = 127;

/// How wide a line of `tcp` is: Linux's `TMPSZ - 1`.
pub const WIDE: usize = 149;

/// The first header line of `/proc/net/dev`.
pub const DEV_HEADER_ONE: &[u8] =
    b"Inter-|   Receive                                                |  Transmit\n";

/// The second.
pub const DEV_HEADER_TWO: &[u8] = b" face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";

/// The header of `/proc/net/route`.
pub const ROUTE_HEADER: &str =
    "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT";

/// The header of `/proc/net/tcp` and `/proc/net/tcp6`.
pub const TCP_HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";

/// The header of `/proc/net/udp` and `/proc/net/udp6`.
pub const UDP_HEADER: &str = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops";

/// The header of `/proc/net/arp`.
pub const ARP_HEADER: &[u8] =
    b"IP address       HW type     Flags       HW address            Mask     Device\n";

/// An IPv4 address as these files spell it: the network-order bytes read as a
/// host-order number, printed in eight hexadecimal digits.
#[must_use]
pub fn hex_v4(octets: [u8; 4]) -> u32 {
    u32::from_le_bytes(octets)
}

/// What one interface has carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceCounters {
    /// Bytes received.
    pub received_bytes: u64,
    /// Packets received.
    pub received: u64,
    /// Packets that arrived and were not understood.
    pub received_errors: u64,
    /// Packets dropped on the way in.
    pub received_dropped: u64,
    /// Multicast packets received.
    pub multicast: u64,
    /// Bytes sent.
    pub sent_bytes: u64,
    /// Packets sent.
    pub sent: u64,
    /// Packets that could not be sent.
    pub sent_errors: u64,
    /// Packets dropped rather than sent.
    pub sent_dropped: u64,
}

/// One row of `/proc/net/dev`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device<'a> {
    /// The interface's name.
    pub name: &'a [u8],
    /// What it has carried.
    pub counters: DeviceCounters,
}

/// Write `/proc/net/dev`: two header lines and a row per interface.
pub fn dev(out: &mut Vec<u8>, devices: &[Device<'_>]) {
    out.extend_from_slice(DEV_HEADER_ONE);
    out.extend_from_slice(DEV_HEADER_TWO);
    for device in devices {
        // The name is right-aligned in six columns and then a colon, which is
        // what `%6s: ` gives; a longer name simply pushes the rest along, as
        // it does on Linux.
        let name_len = device.name.len();
        for _ in name_len..6 {
            out.push(b' ');
        }
        out.extend_from_slice(device.name);
        let counters = device.counters;
        put(
            out,
            format_args!(
                ": {:7} {:7} {:4} {:4} {:4} {:5} {:10} {:9} {:8} {:7} {:4} {:4} {:4} {:5} {:7} {:10}\n",
                counters.received_bytes,
                counters.received,
                counters.received_errors,
                counters.received_dropped,
                0,
                0,
                0,
                counters.multicast,
                counters.sent_bytes,
                counters.sent,
                counters.sent_errors,
                counters.sent_dropped,
                0,
                0,
                0,
                0,
            ),
        );
    }
}

/// One row of `/proc/net/route`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route<'a> {
    /// The interface it goes out of.
    pub interface: &'a [u8],
    /// The network, in network order.
    pub destination: [u8; 4],
    /// The next hop, all zero for a route that is on the link.
    pub gateway: [u8; 4],
    /// `RTF_` flags: `UP` is 1, `GATEWAY` 2, `HOST` 4.
    pub flags: u16,
    /// Lower is preferred.
    pub metric: u32,
    /// The network's mask, in network order.
    pub mask: [u8; 4],
    /// The interface's MTU.
    pub mtu: u32,
}

/// Write `/proc/net/route`.
pub fn route(out: &mut Vec<u8>, routes: &[Route<'_>]) {
    padded(out, NARROW, |line| {
        line.extend_from_slice(ROUTE_HEADER.as_bytes());
    });
    for entry in routes {
        padded(out, NARROW, |line| {
            line.extend_from_slice(entry.interface);
            put(
                line,
                format_args!(
                    "\t{:08X}\t{:08X}\t{:04X}\t0\t0\t{}\t{:08X}\t{}\t0\t0",
                    hex_v4(entry.destination),
                    hex_v4(entry.gateway),
                    entry.flags,
                    entry.metric,
                    hex_v4(entry.mask),
                    entry.mtu,
                ),
            );
        });
    }
}

/// An address and a port, as one of these files spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// An IPv4 address.
    V4([u8; 4], u16),
    /// An IPv6 address.
    V6([u8; 16], u16),
}

/// One row of `/proc/net/tcp` or `/proc/net/udp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Socket {
    /// Which row this is.
    pub slot: usize,
    /// What it is bound to.
    pub local: Endpoint,
    /// What it is connected to.
    pub remote: Endpoint,
    /// The state, as Linux numbers them: 1 established, 10 listening.
    pub state: u8,
    /// Bytes waiting to be sent.
    pub transmit_queue: u32,
    /// Bytes waiting to be read.
    pub receive_queue: u32,
    /// The owner's user id.
    pub uid: u32,
    /// The socket's inode number.
    pub inode: u64,
}

/// Write an endpoint's hexadecimal form into `out`.
fn endpoint(out: &mut Vec<u8>, at: Endpoint) {
    match at {
        Endpoint::V4(octets, port) => {
            put(out, format_args!("{:08X}:{:04X}", hex_v4(octets), port));
        }
        Endpoint::V6(octets, port) => {
            for word in octets.chunks_exact(4) {
                let bytes = [
                    *word.first().unwrap_or(&0),
                    *word.get(1).unwrap_or(&0),
                    *word.get(2).unwrap_or(&0),
                    *word.get(3).unwrap_or(&0),
                ];
                put(out, format_args!("{:08X}", u32::from_le_bytes(bytes)));
            }
            put(out, format_args!(":{port:04X}"));
        }
    }
}

/// Write `/proc/net/tcp` or `/proc/net/tcp6`.
pub fn tcp(out: &mut Vec<u8>, sockets: &[Socket]) {
    padded(out, WIDE, |line| {
        line.extend_from_slice(TCP_HEADER.as_bytes());
    });
    for socket in sockets {
        padded(out, WIDE, |line| {
            put(line, format_args!("{:4}: ", socket.slot));
            endpoint(line, socket.local);
            line.push(b' ');
            endpoint(line, socket.remote);
            put(
                line,
                format_args!(
                    " {:02X} {:08X}:{:08X} 00:00000000 00000000 {:5} {:8} {} 1 0000000000000000 100 0 0 10 0",
                    socket.state,
                    socket.transmit_queue,
                    socket.receive_queue,
                    socket.uid,
                    0,
                    socket.inode,
                ),
            );
        });
    }
}

/// Write `/proc/net/udp` or `/proc/net/udp6`.
pub fn udp(out: &mut Vec<u8>, sockets: &[Socket]) {
    padded(out, NARROW, |line| {
        line.extend_from_slice(UDP_HEADER.as_bytes());
    });
    for socket in sockets {
        padded(out, NARROW, |line| {
            put(line, format_args!("{:5}: ", socket.slot));
            endpoint(line, socket.local);
            line.push(b' ');
            endpoint(line, socket.remote);
            put(
                line,
                format_args!(
                    " {:02X} {:08X}:{:08X} 00:00000000 00000000 {:5} {:8} {} 2 0000000000000000 0",
                    socket.state,
                    socket.transmit_queue,
                    socket.receive_queue,
                    socket.uid,
                    0,
                    socket.inode,
                ),
            );
        });
    }
}

/// One row of `/proc/net/arp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbour<'a> {
    /// The address, as text.
    pub address: [u8; 4],
    /// The `NUD_` bits, which `arp` prints as the flags.
    pub flags: u32,
    /// The hardware address.
    pub hardware: [u8; 6],
    /// The interface it is on.
    pub interface: &'a [u8],
}

/// Write `/proc/net/arp`.
pub fn arp(out: &mut Vec<u8>, neighbours: &[Neighbour<'_>]) {
    out.extend_from_slice(ARP_HEADER);
    for entry in neighbours {
        let [a, b, c, d] = entry.address;
        let mut text = Vec::new();
        put(&mut text, format_args!("{a}.{b}.{c}.{d}"));
        out.extend_from_slice(&text);
        for _ in text.len()..16 {
            out.push(b' ');
        }
        // The hardware type is `ARPHRD_ETHER`, which is what every row this
        // kernel writes is on.
        put(out, format_args!(" 0x{:<10x}0x{:<10x}", 1, entry.flags));
        let [m0, m1, m2, m3, m4, m5] = entry.hardware;
        put(
            out,
            format_args!("{m0:02x}:{m1:02x}:{m2:02x}:{m3:02x}:{m4:02x}:{m5:02x}"),
        );
        out.extend_from_slice(b"     *        ");
        out.extend_from_slice(entry.interface);
        out.push(b'\n');
    }
}

/// Write one line through `body`, padded to `width` characters and ended with
/// a newline.
///
/// This is Linux's `seq_pad`: a line shorter than the width is padded and one
/// longer is left alone, which is why an IPv6 row is longer than an IPv4 one
/// in the same file.
fn padded(out: &mut Vec<u8>, width: usize, body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    body(out);
    for _ in (out.len() - start)..width {
        out.push(b' ');
    }
    out.push(b'\n');
}
