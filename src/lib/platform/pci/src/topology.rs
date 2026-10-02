//! Where a function sits below the bridges enumeration found, as the IOMMU
//! descriptions need it.
//!
//! Firmware names the devices behind an IOMMU by *path*: a start bus and
//! `(device, function)` hops through bridges (the DMAR's device scopes), or
//! by a bridge and everything beneath it (a DMAR sub-hierarchy scope, which is
//! how QEMU's `q35` names a PCIe root port and the slot behind it). The hops
//! after the first are on buses only configuration space numbers, so
//! following them needs the bridges a walk found, which is what this module
//! is given: a slice of [`Bridge`]s, each with the bus range behind it.
//!
//! # Whose requester ID arrives
//!
//! A function's DMA reaches the IOMMU carrying a requester ID, and that ID
//! is not always the function's own. A PCIe port -- a root port, or a
//! switch's upstream or downstream port -- forwards a transaction with the
//! ID it came with. A PCIe-to-PCI bridge, and a conventional PCI bridge, take
//! ownership of what they forward: the IOMMU sees the bridge's alias for
//! every function behind it. [`behind`] answers only the case where the ID
//! is the function's own, and says [`Behind::Aliased`] otherwise, so that a
//! caller never builds a domain for an ID no transaction carries.

use crate::Address;

/// What a function behind a bridge looks like from above it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bridge {
    /// The bridge.
    pub address: Address,
    /// The bus directly behind it.
    pub secondary: u8,
    /// The highest-numbered bus behind it.
    pub subordinate: u8,
    /// Whether it is a PCIe port, which forwards a requester ID unchanged:
    /// a root port, or a switch's upstream or downstream port.
    pub forwards_requester: bool,
}

impl Bridge {
    /// Whether `bus` is behind the bridge.
    #[must_use]
    pub const fn covers(self, bus: u8) -> bool {
        self.secondary <= bus && bus <= self.subordinate
    }
}

/// Follow a path of `(device, function)` hops from `start_bus` on `segment`
/// to the function it names: the first hop is on the start bus, and each
/// later one on the bus behind the bridge the hop before it named.
///
/// `None` for an empty path, a hop through a function that is not one of
/// `bridges`, or a device or function number out of range.
#[must_use]
pub fn follow_path(
    segment: u16,
    start_bus: u8,
    hops: impl IntoIterator<Item = (u8, u8)>,
    bridges: &[Bridge],
) -> Option<Address> {
    let mut at: Option<Address> = None;
    for (device, function) in hops {
        let bus = match at {
            None => start_bus,
            Some(previous) => {
                bridges
                    .iter()
                    .find(|bridge| bridge.address == previous)?
                    .secondary
            }
        };
        at = Some(Address::new(segment, bus, device, function)?);
    }
    at
}

/// Where `function` stands relative to the bridge at `top`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Behind {
    /// Not below it.
    No,
    /// Below it, and its own requester ID is what arrives above it.
    Own,
    /// Below it, through a bridge that puts its own alias on what it
    /// forwards, or through bus numbers no bridge here explains.
    Aliased,
}

/// Whether `function` is below the bridge at `top`, and with which
/// requester ID its DMA leaves `top`.
///
/// Walks up from the function's bus: each bus is the secondary bus of the
/// bridge directly above it, and every bridge on the way, `top` included,
/// must forward requester IDs unchanged for the answer to be [`Behind::Own`].
/// A walk that does not reach `top` in at most 256 steps -- a loop of bus
/// numbers firmware got wrong -- is [`Behind::Aliased`], never `Own`.
#[must_use]
pub fn behind(function: Address, top: Address, bridges: &[Bridge]) -> Behind {
    let Some(head) = bridges.iter().find(|bridge| bridge.address == top) else {
        return Behind::No;
    };
    if function.segment() != top.segment() || !head.covers(function.bus()) {
        return Behind::No;
    }
    let mut bus = function.bus();
    for _ in 0..=u8::MAX as usize {
        let Some(above) = bridges
            .iter()
            .find(|bridge| bridge.address.segment() == top.segment() && bridge.secondary == bus)
        else {
            return Behind::Aliased;
        };
        if !above.forwards_requester {
            return Behind::Aliased;
        }
        if above.address == top {
            return Behind::Own;
        }
        bus = above.address.bus();
    }
    Behind::Aliased
}

#[cfg(test)]
mod tests {
    use super::{Behind, Bridge, behind, follow_path};
    use crate::Address;

    fn at(bus: u8, device: u8, function: u8) -> Address {
        Address::new(0, bus, device, function).unwrap()
    }

    /// QEMU's `q35` as libvirt lays it out: two root ports on bus 0, a switch
    /// below the second, and a PCIe-to-PCI bridge below the first port.
    fn machine() -> [Bridge; 5] {
        [
            Bridge {
                address: at(0, 2, 0),
                secondary: 1,
                subordinate: 2,
                forwards_requester: true,
            },
            Bridge {
                address: at(0, 2, 1),
                secondary: 3,
                subordinate: 5,
                forwards_requester: true,
            },
            // A PCIe-to-PCI bridge behind the first port.
            Bridge {
                address: at(1, 0, 0),
                secondary: 2,
                subordinate: 2,
                forwards_requester: false,
            },
            // A switch: its upstream port on bus 3, a downstream port on 4.
            Bridge {
                address: at(3, 0, 0),
                secondary: 4,
                subordinate: 5,
                forwards_requester: true,
            },
            Bridge {
                address: at(4, 1, 0),
                secondary: 5,
                subordinate: 5,
                forwards_requester: true,
            },
        ]
    }

    #[test]
    fn a_one_hop_path_names_a_function_on_the_start_bus() {
        assert_eq!(follow_path(0, 0, [(2, 1)], &machine()), Some(at(0, 2, 1)));
    }

    /// Verifies: L.iommu.45
    #[test]
    fn a_longer_path_is_followed_through_each_bridge_s_secondary_bus() {
        let bridges = machine();
        assert_eq!(
            follow_path(0, 0, [(2, 1), (0, 0), (1, 0), (0, 0)], &bridges),
            Some(at(5, 0, 0))
        );
    }

    #[test]
    fn a_path_through_something_that_is_not_a_bridge_names_nothing() {
        assert_eq!(follow_path(0, 0, [(7, 0), (0, 0)], &machine()), None);
        assert_eq!(follow_path(0, 0, [], &machine()), None);
        assert_eq!(follow_path(0, 0, [(32, 0)], &machine()), None);
    }

    /// Verifies: L.iommu.45
    #[test]
    fn an_endpoint_behind_a_root_port_keeps_its_own_requester_id() {
        assert_eq!(behind(at(3, 0, 0), at(0, 2, 1), &machine()), Behind::Own);
    }

    /// Verifies: L.iommu.45
    #[test]
    fn an_endpoint_behind_a_switch_keeps_its_own_requester_id() {
        assert_eq!(behind(at(5, 0, 0), at(0, 2, 1), &machine()), Behind::Own);
        assert_eq!(behind(at(5, 0, 0), at(3, 0, 0), &machine()), Behind::Own);
    }

    /// Verifies: L.iommu.45
    #[test]
    fn behind_a_pcie_to_pci_bridge_the_requester_id_is_an_alias() {
        assert_eq!(
            behind(at(2, 3, 0), at(0, 2, 0), &machine()),
            Behind::Aliased
        );
        // The bridge itself, on bus 1, is reached through the port alone.
        assert_eq!(behind(at(1, 0, 0), at(0, 2, 0), &machine()), Behind::Own);
    }

    #[test]
    fn a_function_outside_the_bridge_s_range_is_not_behind_it() {
        assert_eq!(behind(at(3, 0, 0), at(0, 2, 0), &machine()), Behind::No);
        assert_eq!(behind(at(0, 1, 0), at(0, 2, 0), &machine()), Behind::No);
        // The bridge itself is on its primary bus, not behind itself.
        assert_eq!(behind(at(0, 2, 0), at(0, 2, 0), &machine()), Behind::No);
        // Something that is not a bridge has nothing behind it.
        assert_eq!(behind(at(1, 0, 0), at(0, 7, 0), &machine()), Behind::No);
        // Another segment's bus 3 is not this one's.
        assert_eq!(
            behind(Address::new(1, 3, 0, 0).unwrap(), at(0, 2, 1), &machine()),
            Behind::No
        );
    }

    /// Verifies: L.iommu.45
    #[test]
    fn a_bus_no_bridge_explains_is_never_taken_for_the_function_s_own() {
        // Firmware gave the port a range wider than its children account for:
        // bus 2 is inside 0:2.0's range, but its own bridge is withheld.
        let bridges = [machine()[0]];
        assert_eq!(behind(at(2, 0, 0), at(0, 2, 0), &bridges), Behind::Aliased);
    }

    /// Verifies: L.iommu.45
    #[test]
    fn a_loop_of_bus_numbers_ends_aliased() {
        // Two bridges naming each other's buses, as broken firmware could.
        let bridges = [
            Bridge {
                address: at(0, 2, 0),
                secondary: 1,
                subordinate: 9,
                forwards_requester: true,
            },
            Bridge {
                address: at(2, 0, 0),
                secondary: 3,
                subordinate: 3,
                forwards_requester: true,
            },
            Bridge {
                address: at(3, 0, 0),
                secondary: 2,
                subordinate: 2,
                forwards_requester: true,
            },
        ];
        assert_eq!(behind(at(3, 1, 0), at(0, 2, 0), &bridges), Behind::Aliased);
    }
}
