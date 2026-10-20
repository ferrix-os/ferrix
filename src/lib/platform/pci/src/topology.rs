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
        let above = match above(top.segment(), bus, bridges) {
            Above::One(above) if above.forwards_requester => above,
            Above::One(_) | Above::None | Above::Ambiguous => return Behind::Aliased,
        };
        if above.address == top {
            return Behind::Own;
        }
        bus = above.address.bus();
    }
    Behind::Aliased
}

/// The bridge directly above `bus` on `segment`.
enum Above {
    /// None: `bus` is a root bus, or one no bridge explains.
    None,
    /// Exactly one.
    One(Bridge),
    /// Two bridges claim it, as firmware that numbered them wrong could.
    Ambiguous,
}

/// The bridge whose secondary bus is `bus`.
fn above(segment: u16, bus: u8, bridges: &[Bridge]) -> Above {
    let mut found = bridges
        .iter()
        .filter(|bridge| bridge.address.segment() == segment && bridge.secondary == bus);
    match (found.next(), found.next()) {
        (None, _) => Above::None,
        (Some(&bridge), None) => Above::One(bridge),
        (Some(_), Some(_)) => Above::Ambiguous,
    }
}

/// Whether `function`'s DMA reaches its root bus carrying its own requester
/// ID: every bridge above it is a PCIe port, up to a bus no bridge has
/// behind it.
///
/// What an IOMMU description naming `function` by a path, a sub-hierarchy or
/// an endpoint on its root bus alike, must also satisfy before a domain is
/// built for that ID: a function behind a PCIe-to-PCI or conventional
/// bridge arrives as the bridge's alias, and two such functions would share
/// a domain. A bus two bridges claim, and a loop of bus numbers that does
/// not end in 256 steps, are not its own.
///
/// # Its premise
///
/// A bus no bridge has behind it is taken for a root bus. That holds for
/// the functions a bridge-following walk found -- the walk reaches a bus
/// only through the bridge whose secondary it is, so every bus it scanned
/// but a root bus has its bridge in `bridges` -- and for nothing else: on a
/// bus inside a bridge's range that no bridge explains, this answers `true`.
/// So it is never the whole answer for a function below a named bridge;
/// [`claims`] asks [`behind`] as well, which calls such a bus aliased.
#[must_use]
pub fn own_to_root(function: Address, bridges: &[Bridge]) -> bool {
    let mut bus = function.bus();
    for _ in 0..=u8::MAX as usize {
        match above(function.segment(), bus, bridges) {
            Above::None => return true,
            Above::One(bridge) if bridge.forwards_requester => bus = bridge.address.bus(),
            Above::One(_) | Above::Ambiguous => return false,
        }
    }
    false
}

/// Whether an IOMMU description's scope naming `named` -- a sub-hierarchy
/// (the bridge and everything below it) if `hierarchy`, an endpoint
/// otherwise -- puts `function` behind its unit under `function`'s own
/// requester ID.
///
/// [`Behind::No`] when the scope does not reach `function`: it is not
/// `named`, nor, for a sub-hierarchy, below it. [`Behind::Own`] only when
/// it does, and every bridge from `function` up to its root bus forwards
/// requester IDs unchanged: for a function below `named`, every bridge up to
/// `named` and `named` itself ([`behind`] says [`Behind::Own`]), and every
/// bridge above `named` as well ([`own_to_root`]). Anything else the scope
/// reaches is [`Behind::Aliased`] -- never given a domain, since a second
/// function behind the same aliasing bridge would share it. In particular,
/// [`behind`]'s `Aliased` is never turned into `Own` here, whatever
/// [`own_to_root`]'s root-bus premise says of the same bus.
#[must_use]
pub fn claims(hierarchy: bool, named: Address, function: Address, bridges: &[Bridge]) -> Behind {
    let below = if hierarchy && named != function {
        behind(function, named, bridges)
    } else {
        Behind::No
    };
    if named != function && below == Behind::No {
        Behind::No
    } else if below == Behind::Aliased || !own_to_root(function, bridges) {
        Behind::Aliased
    } else {
        Behind::Own
    }
}

/// What a DMAR's explicit scopes -- endpoints and sub-hierarchies, of every
/// unit on the segment, refused ones included -- say of one function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Explicit<U> {
    /// A scope of unit `U` reaches it as its own requester.
    Own(U),
    /// None does, but one reaches it under an alias, or a scope's path could
    /// not be followed: it may be one of those.
    Unfollowed,
    /// No scope reaches it.
    None,
}

/// A segment's catch-all unit, as the DMAR says (`INCLUDE_PCI_ALL`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CatchAll<U> {
    /// None.
    None,
    /// Exactly one: unit `U`.
    One(U),
    /// More than one, which is not allowed: none is believed.
    Several,
}

/// Where a function is placed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placed<U> {
    /// Behind unit `U`, as its own requester ID.
    Unit(U),
    /// Behind no unit.
    Nothing,
    /// Firmware says something that cannot be followed: never given a domain.
    Unresolved,
}

/// Where `function` is placed, from what the explicit scopes said and the
/// segment's catch-all: the one decision both the discovery report and the
/// domains ask, so the two cannot disagree.
///
/// An explicit scope outranks the catch-all, whether or not its unit came
/// up: a function a refused unit's scope names is behind that unit, not the
/// catch-all. A function no explicit scope reaches is the catch-all's only
/// when its own requester ID reaches its root bus ([`own_to_root`]): one
/// behind a PCIe-to-PCI bridge arrives as the bridge's alias and is
/// unresolved, as two catch-alls make every such function.
#[must_use]
pub fn place<U>(
    explicit: Explicit<U>,
    catch_all: CatchAll<U>,
    function: Address,
    bridges: &[Bridge],
) -> Placed<U> {
    match explicit {
        Explicit::Own(unit) => Placed::Unit(unit),
        Explicit::Unfollowed => Placed::Unresolved,
        Explicit::None => match catch_all {
            CatchAll::None => Placed::Nothing,
            CatchAll::Several => Placed::Unresolved,
            CatchAll::One(unit) if own_to_root(function, bridges) => Placed::Unit(unit),
            CatchAll::One(_) => Placed::Unresolved,
        },
    }
}

/// Whether `placed` puts a function behind `unit`, the unit an RMRR's
/// identity domain was attached on for it at bring-up, from the DMAR alone:
/// only then is that domain the function's. A function enumeration places
/// elsewhere, under an alias or not at all gets none (`L.iommu.78`).
pub fn agrees<U: PartialEq>(placed: Placed<U>, unit: &U) -> bool {
    matches!(placed, Placed::Unit(at) if at == *unit)
}

#[cfg(test)]
mod tests {
    use super::{
        Behind, Bridge, CatchAll, Explicit, Placed, agrees, behind, claims, follow_path,
        own_to_root, place,
    };
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

    /// An endpoint named by a path through a PCIe-to-PCI bridge is never its
    /// own requester: the bridge puts its alias on what it forwards, and two
    /// such endpoints would share one domain.
    ///
    /// Verifies: L.iommu.45
    #[test]
    fn an_endpoint_behind_a_pcie_to_pci_bridge_is_never_its_own() {
        let bridges = machine();
        // 0:2.0 (root port) -> 1:0.0 (PCIe-to-PCI) -> bus 2.
        let named = follow_path(0, 0, [(2, 0), (0, 0), (3, 0)], &bridges);
        assert_eq!(named, Some(at(2, 3, 0)));
        assert!(!own_to_root(at(2, 3, 0), &bridges));
        assert!(!own_to_root(at(2, 4, 0), &bridges));
        // Behind the root port alone, and behind a switch, it is.
        assert!(own_to_root(at(1, 0, 0), &bridges));
        assert!(own_to_root(at(5, 0, 0), &bridges));
        // On the root bus nothing is above it.
        assert!(own_to_root(at(0, 3, 0), &bridges));
    }

    /// Two bridges claiming one secondary bus leave what is on it aliased.
    ///
    /// Verifies: L.iommu.45
    #[test]
    fn a_bus_two_bridges_claim_is_never_its_own() {
        let mut bridges = machine().to_vec();
        bridges.push(Bridge {
            address: at(0, 7, 0),
            secondary: 3,
            subordinate: 5,
            forwards_requester: true,
        });
        assert!(!own_to_root(at(3, 0, 0), &bridges));
        assert_eq!(behind(at(3, 0, 0), at(0, 2, 1), &bridges), Behind::Aliased);
    }

    /// Verifies: L.iommu.45
    #[test]
    fn a_bus_no_bridge_explains_is_never_taken_for_the_function_s_own() {
        // Firmware gave the port a range wider than its children account for:
        // bus 2 is inside 0:2.0's range, but its own bridge is withheld.
        let bridges = [machine()[0]];
        assert_eq!(behind(at(2, 0, 0), at(0, 2, 0), &bridges), Behind::Aliased);
    }

    /// An endpoint scope whose path runs through a PCIe-to-PCI bridge
    /// reaches the function it names, and claims it aliased, never as its
    /// own: the bridge puts its own requester ID on what it forwards.
    ///
    /// Verifies: L.iommu.45
    #[test]
    fn claims_a_two_hop_endpoint_through_a_pcie_to_pci_bridge_as_aliased() {
        let bridges = machine();
        // 0:2.0 (root port) -> 1:0.0 (PCIe-to-PCI, type 7) -> 2:3.0.
        let named = follow_path(0, 0, [(2, 0), (0, 0), (3, 0)], &bridges).unwrap();
        assert_eq!(named, at(2, 3, 0));
        assert_eq!(claims(false, named, named, &bridges), Behind::Aliased);
        // A sub-hierarchy over the PCIe-to-PCI bridge, or over the port
        // above it, reaches the same function aliased too.
        assert_eq!(claims(true, at(1, 0, 0), named, &bridges), Behind::Aliased);
        assert_eq!(claims(true, at(0, 2, 0), named, &bridges), Behind::Aliased);
        // The bridge itself is reached through the port alone, as its own.
        assert_eq!(
            claims(true, at(0, 2, 0), at(1, 0, 0), &bridges),
            Behind::Own
        );
    }

    /// A sub-hierarchy scope over a bus inside the named bridge's range that
    /// no bridge explains claims what is on it aliased -- where
    /// `own_to_root`, taking that bus for a root bus, would say its own.
    ///
    /// Verifies: L.iommu.45
    #[test]
    fn claims_a_function_on_a_bus_no_bridge_explains_as_aliased() {
        // The port's range is buses 1-2, but bus 2's bridge is withheld.
        let bridges = [machine()[0]];
        assert!(own_to_root(at(2, 0, 0), &bridges));
        assert_eq!(behind(at(2, 0, 0), at(0, 2, 0), &bridges), Behind::Aliased);
        assert_eq!(
            claims(true, at(0, 2, 0), at(2, 0, 0), &bridges),
            Behind::Aliased
        );
    }

    /// What a scope reaches through PCIe ports alone it claims as the
    /// function's own; what it does not reach it does not claim.
    ///
    /// Verifies: L.iommu.45
    #[test]
    fn claims_through_pcie_ports_as_own_and_nothing_outside_the_scope() {
        let bridges = machine();
        // A sub-hierarchy over a root port, below a switch.
        assert_eq!(
            claims(true, at(0, 2, 1), at(5, 0, 0), &bridges),
            Behind::Own
        );
        assert_eq!(
            claims(true, at(3, 0, 0), at(5, 0, 0), &bridges),
            Behind::Own
        );
        // An endpoint on the root bus, and one named by a path through ports.
        assert_eq!(
            claims(false, at(0, 3, 0), at(0, 3, 0), &bridges),
            Behind::Own
        );
        let switched = follow_path(0, 0, [(2, 1), (0, 0), (1, 0), (0, 0)], &bridges).unwrap();
        assert_eq!(claims(false, switched, switched, &bridges), Behind::Own);
        // Not reached: another endpoint, a function outside the range, and
        // an endpoint scope never reaching below what it names.
        assert_eq!(
            claims(false, at(0, 3, 0), at(0, 4, 0), &bridges),
            Behind::No
        );
        assert_eq!(claims(true, at(0, 2, 0), at(3, 0, 0), &bridges), Behind::No);
        assert_eq!(
            claims(false, at(0, 2, 1), at(5, 0, 0), &bridges),
            Behind::No
        );
    }

    /// A function on a bus two bridges claim, reached by a sub-hierarchy
    /// scope, is aliased, never its own.
    ///
    /// Verifies: L.iommu.45
    #[test]
    fn claims_a_function_on_a_bus_two_bridges_claim_as_aliased() {
        let mut bridges = machine().to_vec();
        bridges.push(Bridge {
            address: at(0, 7, 0),
            secondary: 3,
            subordinate: 5,
            forwards_requester: true,
        });
        assert_eq!(
            claims(true, at(0, 2, 1), at(3, 0, 0), &bridges),
            Behind::Aliased
        );
        assert_eq!(
            claims(false, at(3, 0, 0), at(3, 0, 0), &bridges),
            Behind::Aliased
        );
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
        assert!(!own_to_root(at(3, 1, 0), &bridges));
    }

    /// The N150 box's bridges, from its `lspci` (`box-2026-10-16/lspci.txt`):
    /// two PCIe root ports, 00:1c.0 with the YT6801 on bus 1 and 00:1c.6
    /// with the WLAN on bus 2.
    fn box_bridges() -> [Bridge; 2] {
        [
            Bridge {
                address: at(0, 0x1C, 0),
                secondary: 1,
                subordinate: 1,
                forwards_requester: true,
            },
            Bridge {
                address: at(0, 0x1C, 6),
                secondary: 2,
                subordinate: 2,
                forwards_requester: true,
            },
        ]
    }

    /// The box's 27 functions (`lspci.txt`).
    const BOX_FUNCTIONS: [(u8, u8, u8); 27] = [
        (0, 0x00, 0),
        (0, 0x02, 0),
        (0, 0x04, 0),
        (0, 0x0A, 0),
        (0, 0x0D, 0),
        (0, 0x14, 0),
        (0, 0x14, 2),
        (0, 0x15, 0),
        (0, 0x15, 1),
        (0, 0x15, 2),
        (0, 0x15, 3),
        (0, 0x16, 0),
        (0, 0x17, 0),
        (0, 0x19, 0),
        (0, 0x19, 1),
        (0, 0x1A, 0),
        (0, 0x1C, 0),
        (0, 0x1C, 6),
        (0, 0x1E, 0),
        (0, 0x1E, 2),
        (0, 0x1E, 3),
        (0, 0x1F, 0),
        (0, 0x1F, 3),
        (0, 0x1F, 4),
        (0, 0x1F, 5),
        (1, 0x00, 0),
        (2, 0x00, 0),
    ];

    /// Verifies: `L.iommu.62`
    #[test]
    fn every_box_function_is_placed_behind_a_unit() {
        let bridges = box_bridges();
        let mut behind_dmar1 = 0;
        for (bus, device, function) in BOX_FUNCTIONS {
            let address = at(bus, device, function);
            // dmar0's one endpoint scope names the iGPU.
            let explicit = if (bus, device, function) == (0, 2, 0) {
                Explicit::Own("dmar0")
            } else {
                Explicit::None
            };
            match place(explicit, CatchAll::One("dmar1"), address, &bridges) {
                Placed::Unit("dmar0") => assert_eq!(address, at(0, 2, 0)),
                Placed::Unit("dmar1") => behind_dmar1 += 1,
                other => panic!("{address}: {other:?}"),
            }
        }
        assert_eq!(
            behind_dmar1, 26,
            "26 behind dmar1, 1 behind dmar0, 0 bypassing"
        );
        assert_eq!(
            place(
                Explicit::None,
                CatchAll::One("dmar1"),
                at(1, 0, 0),
                &bridges
            ),
            Placed::Unit("dmar1"),
            "the YT6801 below root port 00:1c.0, as its own requester"
        );
    }

    /// Verifies: `L.iommu.62`
    #[test]
    fn the_catch_all_takes_no_alias_and_two_take_nothing() {
        let bridges = machine();
        // Bus 2 is behind the PCIe-to-PCI bridge at 01:00.0 of `machine`.
        let aliased = at(2, 1, 0);
        assert_eq!(
            place(Explicit::None, CatchAll::One(1), aliased, &bridges),
            Placed::Unresolved,
            "behind a PCIe-to-PCI bridge"
        );
        assert_eq!(
            place(
                Explicit::None,
                CatchAll::Several::<u8>,
                at(0, 3, 0),
                &bridges
            ),
            Placed::Unresolved
        );
        assert_eq!(
            place(Explicit::None, CatchAll::None::<u8>, at(0, 3, 0), &bridges),
            Placed::Nothing
        );
        assert_eq!(
            place(
                Explicit::Unfollowed,
                CatchAll::One(1),
                at(0, 3, 0),
                &bridges
            ),
            Placed::Unresolved,
            "an unfollowed scope is never the catch-all's"
        );
    }

    /// Verifies: `L.iommu.63`
    #[test]
    fn an_explicit_scope_outranks_the_catch_all() {
        // Unit `None` stands for a refused unit, as the kernel records one.
        assert_eq!(
            place(
                Explicit::Own(None),
                CatchAll::One(Some(1)),
                at(0, 2, 0),
                &box_bridges()
            ),
            Placed::Unit(None),
            "a refused unit's function is not handed to the catch-all"
        );
    }

    /// Verifies: `L.iommu.78`
    #[test]
    fn an_identity_domain_is_its_functions_only_where_enumeration_agrees() {
        let bridges = box_bridges();
        // The box: the iGPU is dmar0's (index 0) by its endpoint scope.
        let igpu = place(
            Explicit::Own(Some(0)),
            CatchAll::One(Some(1)),
            at(0, 2, 0),
            &bridges,
        );
        assert!(agrees(igpu, &Some(0)), "00:02.0 behind dmar0");
        assert!(!agrees(igpu, &Some(1)), "not behind dmar1");
        // Placed by the catch-all instead: not the unit the domain is on.
        let elsewhere = place(
            Explicit::None,
            CatchAll::One(Some(1)),
            at(0, 2, 0),
            &bridges,
        );
        assert!(!agrees(elsewhere, &Some(0)));
        // Unresolved, under an alias, or behind nothing: no domain.
        let aliased = place(
            Explicit::None,
            CatchAll::One(Some(0)),
            at(2, 1, 0),
            &machine(),
        );
        assert_eq!(aliased, Placed::Unresolved);
        assert!(!agrees(aliased, &Some(0)));
        assert!(!agrees(Placed::Nothing, &Some(0)));
        assert!(!agrees(Placed::Unit(None), &Some(0)), "a refused unit");
    }
}
