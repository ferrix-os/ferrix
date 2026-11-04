//! Where a frame goes, in `netd` (`docs/microkernel/M8.md` §2). Layer 2 only, and
//! this is all of it: ARP and IP spoofing between partitions remain possible, as on a wire.
//!
//! **The outside sees one machine** (customer, 2026-11-04): one address, one MAC, one DHCP
//! client. So there are two links that never meet. Port 0, the uplink, is the first
//! partition's interface with the wire's own MAC, and goes to the wire and nowhere else.
//! Every other port is on the internal link, which has no way to the wire: the first
//! partition's second interface and each further partition's only one. The first partition's
//! ARP for an internal address therefore never reaches the wire, and no wire host can answer it.
//!
//! The table is fixed at the start, each port one MAC: it is **never learned** from what a
//! port sends, so no partition can draw another's traffic to it. A frame whose source MAC
//! is not its port's is dropped. No unicast reaches any port but its owner, and there is
//! no flood of unicast.

#![no_std]

/// The most partitions.
pub const MAX_PORTS: usize = 8;
/// The least a frame can be and name two MACs.
pub const HEADER: usize = 12;

/// A MAC address.
pub type Mac = [u8; 6];

/// Which ports a frame goes to, and whether the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Route {
    /// A bit per partition.
    pub ports: u8,
    /// Out on the wire.
    pub wire: bool,
}

/// Nothing.
pub const DROP: Route = Route {
    ports: 0,
    wire: false,
};

/// The table.
#[derive(Clone, Copy, Debug)]
pub struct Switch {
    macs: [Mac; MAX_PORTS],
    count: usize,
}

/// The MAC port `index` has: the wire's for the uplink, so that one partition
/// sees exactly the interface it would alone (ledger 6226 N.3), and for the others that
/// address with the locally administered bit set and the index in its last byte.
#[must_use]
pub const fn mac_of(wire: Mac, index: usize) -> Mac {
    if index == 0 {
        return wire;
    }
    let [a, b, c, d, e, f] = wire;
    [(a | 0x02) & 0xFE, b, c, d, e, f ^ (index as u8)]
}

fn all_bits(count: usize) -> u8 {
    if count >= 8 {
        0xFF
    } else {
        (1_u8 << count) - 1
    }
}

impl Switch {
    /// A switch of `count` partitions over the wire's address; `count` past
    /// [`MAX_PORTS`] is that many.
    #[must_use]
    pub fn new(wire: Mac, count: usize) -> Switch {
        let count = count.min(MAX_PORTS);
        let mut macs = [[0; 6]; MAX_PORTS];
        for (index, mac) in macs.iter_mut().enumerate().take(count) {
            *mac = mac_of(wire, index);
        }
        Switch { macs, count }
    }

    /// How many partitions.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Partition `index`'s MAC.
    #[must_use]
    pub fn mac(&self, index: usize) -> Option<Mac> {
        (index < self.count)
            .then(|| self.macs.get(index).copied())
            .flatten()
    }

    fn owner(&self, mac: Mac) -> Option<usize> {
        self.macs
            .iter()
            .take(self.count)
            .position(|&known| known == mac)
    }

    /// A frame port `from` sent. The uplink's goes to the wire alone, an internal port's
    /// never does.
    #[must_use]
    pub fn from_partition(&self, from: usize, frame: &[u8]) -> Route {
        let Some((destination, source)) = macs(frame) else {
            return DROP;
        };
        if from >= self.count || self.mac(from) != Some(source) {
            return DROP;
        }
        if from == 0 {
            return Route {
                ports: 0,
                wire: true,
            };
        }
        if destination[0] & 1 != 0 {
            return Route {
                ports: all_bits(self.count) & !1 & !(1_u8 << from),
                wire: false,
            };
        }
        match self.owner(destination) {
            Some(owner) if owner == from || owner == 0 => DROP,
            Some(owner) => Route {
                ports: 1 << owner,
                wire: false,
            },
            None => DROP,
        }
    }

    /// A frame from the wire: the uplink's alone.
    #[must_use]
    pub fn from_wire(&self, frame: &[u8]) -> Route {
        let Some((destination, _)) = macs(frame) else {
            return DROP;
        };
        if self.count != 0 && (destination[0] & 1 != 0 || self.owner(destination) == Some(0)) {
            return Route {
                ports: 1,
                wire: false,
            };
        }
        DROP
    }
}

fn macs(frame: &[u8]) -> Option<(Mac, Mac)> {
    let destination = <Mac>::try_from(frame.get(..6)?).ok()?;
    let source = <Mac>::try_from(frame.get(6..12)?).ok()?;
    Some((destination, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIRE: Mac = [0x52, 0x54, 0, 0x12, 0x34, 0x56];

    fn frame(to: Mac, from: Mac) -> [u8; 14] {
        let mut out = [0; 14];
        out[..6].copy_from_slice(&to);
        out[6..12].copy_from_slice(&from);
        out
    }

    #[test]
    fn the_first_partition_has_the_wires_mac_and_the_others_a_local_one() {
        let switch = Switch::new(WIRE, 3);
        assert_eq!(switch.mac(0), Some(WIRE));
        let other = switch.mac(1).expect("a second");
        assert_ne!(other, WIRE);
        assert_eq!(other[0] & 2, 2, "locally administered");
        assert_eq!(other[0] & 1, 0, "unicast");
        assert_ne!(switch.mac(1), switch.mac(2));
        assert_eq!(switch.mac(3), None);
    }

    #[test]
    fn a_frame_with_anothers_source_is_dropped() {
        let switch = Switch::new(WIRE, 2);
        let (zero, one) = (WIRE, switch.mac(1).expect("a second"));
        assert_eq!(switch.from_partition(1, &frame([8; 6], zero)), DROP);
        assert_eq!(switch.from_partition(0, &frame([8; 6], one)), DROP);
    }

    #[test]
    fn an_internal_unicast_reaches_its_owner_alone() {
        let switch = Switch::new(WIRE, 4);
        let (one, two) = (
            switch.mac(1).expect("a port"),
            switch.mac(2).expect("a port"),
        );
        assert_eq!(
            switch.from_partition(1, &frame(two, one)),
            Route {
                ports: 0b100,
                wire: false
            }
        );
    }

    #[test]
    fn the_uplink_goes_to_the_wire_and_nowhere_else() {
        let switch = Switch::new(WIRE, 3);
        let one = switch.mac(1).expect("a port");
        let wire = Route {
            ports: 0,
            wire: true,
        };
        assert_eq!(switch.from_partition(0, &frame([0xFF; 6], WIRE)), wire);
        assert_eq!(switch.from_partition(0, &frame(one, WIRE)), wire);
        assert_eq!(switch.from_partition(0, &frame([8; 6], WIRE)), wire);
    }

    #[test]
    fn the_outside_sees_one_machine() {
        let switch = Switch::new(WIRE, 3);
        let one = switch.mac(1).expect("a port");
        // Nothing of the internal link reaches the wire, or the uplink.
        assert_eq!(switch.from_partition(1, &frame([8; 6], one)), DROP);
        assert_eq!(switch.from_partition(1, &frame(WIRE, one)), DROP);
        let broadcast = switch.from_partition(1, &frame([0xFF; 6], one));
        assert!(!broadcast.wire);
        assert_eq!(broadcast.ports, 0b100);
        // And nothing of the wire reaches it, by its MAC or by a broadcast.
        assert_eq!(switch.from_wire(&frame(one, [7; 6])), DROP);
        assert_eq!(switch.from_wire(&frame([0xFF; 6], [7; 6])).ports, 0b001);
        assert_eq!(switch.from_wire(&frame(WIRE, [7; 6])).ports, 0b001);
        assert_eq!(
            switch.from_wire(&frame([0x02, 1, 2, 3, 4, 5], [7; 6])),
            DROP
        );
    }

    #[test]
    fn nothing_is_learned_from_what_a_port_sends() {
        let switch = Switch::new(WIRE, 2);
        let sent_from = [0x02, 9, 9, 9, 9, 9];
        let _ = switch.from_partition(1, &frame([0xFF; 6], sent_from));
        assert_eq!(switch.from_wire(&frame(sent_from, [7; 6])), DROP);
    }

    #[test]
    fn a_senders_source_teaches_the_table_nothing() {
        let switch = Switch::new(WIRE, 4);
        let (two, three) = (
            switch.mac(2).expect("a port"),
            switch.mac(3).expect("a port"),
        );
        let made_up = [0x02, 9, 9, 9, 9, 9];
        let _ = switch.from_partition(1, &frame([0xFF; 6], made_up));
        let _ = switch.from_partition(1, &frame([0xFF; 6], two));
        assert_eq!(switch.from_partition(3, &frame(made_up, three)), DROP);
        assert_eq!(
            switch.from_partition(3, &frame(two, three)),
            Route {
                ports: 0b100,
                wire: false
            }
        );
    }

    #[test]
    fn a_short_frame_goes_nowhere() {
        let switch = Switch::new(WIRE, 2);
        assert_eq!(switch.from_wire(&[0; 11]), DROP);
        assert_eq!(switch.from_partition(0, &[0; 3]), DROP);
        assert_eq!(switch.from_partition(9, &frame([0xFF; 6], WIRE)), DROP);
    }
}
