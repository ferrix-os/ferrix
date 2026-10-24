//! Stage 10 on an AMD machine: the IVRS's units brought up, every PCI
//! function placed behind one, and the two functions no entry need name
//! (`docs/AMD-VI.md` §2, §4, §9.1).
//!
//! # Placement
//!
//! A unit's IVHD names devids by select, range and alias entries, and may
//! name every devid with an ALL entry. A function is behind the unit whose
//! entries name it as its own -- the last entry naming it decides, as
//! Linux's walk leaves it -- and that enumeration places as its own
//! requester (`ferrix_pci::topology::claims`); one an alias entry names, or
//! one under an aliasing bridge, is unresolved; one a unit's ALL entry
//! reaches is that unit's when no explicit entry names it. The decision is
//! `topology::place`, the one VT-d's placements make, so the report and the
//! domains cannot disagree. An IVRS that cannot be read whole
//! (`ferrix_acpi::ivrs::Ivrs::whole`) places nothing: every function is
//! unresolved.
//!
//! # The two functions no entry need name (D10, ledger 3923)
//!
//! * **The unit's own function**, at its IVHD devid with class 08h/06h: the
//!   unit itself, whose table walks and log writes it never translates.
//!   Counted behind its unit, and never given a domain or a vector.
//! * **The root complex**: a host bridge (class 06h/00h) on bus 0 that no
//!   entry names, with bus mastering off as enumeration found it. It issues
//!   no DMA of its own; it is counted apart, as a root complex, and never
//!   given a domain or a vector. With bus mastering on it is bypassing.

use alloc::vec::Vec;

use ferrix_acpi::ivrs::{self, Entry, Ivrs, Named, SPECIAL_IOAPIC};
use ferrix_paging::vtd::rmrr::{self, Identity};
use ferrix_pci::Address;
use ferrix_pci::topology::{self, Bridge};
use ferrix_sync::Once;

use super::amdvi;
use super::{IdentityDomain, Programmed, claims};
use crate::println;

/// One unit as placement reads it: its segment, its own devid, its device
/// entries, and its index among the programmed units (`None` for one left
/// alone).
#[derive(Debug)]
pub(crate) struct Placing {
    /// The segment it serves.
    segment: u16,
    /// Its own function's devid.
    devid: u16,
    /// Its device entries.
    entries: Vec<Entry>,
    /// Its index in `Programmed::amdvi`.
    pub(crate) index: Option<usize>,
    /// Its registers' physical address.
    pub(crate) base: u64,
}

/// Every unit of `table`, as placement reads them, with no index yet; or
/// none when the table cannot be read whole.
pub(crate) fn placing(table: &Ivrs<'_>) -> Option<Vec<Placing>> {
    if !table.whole() {
        return None;
    }
    let mut units = Vec::new();
    for unit in table.units() {
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        let entries: Vec<Entry> = unit.entries().collect();
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        units.push(Placing {
            segment: unit.segment,
            devid: unit.devid,
            entries,
            index: None,
            base: unit.base,
        });
    }
    Some(units)
}

/// Where `units` put `function`, by position in `units`.
pub(crate) fn placed(
    units: &[Placing],
    function: Address,
    bridges: &[Bridge],
) -> topology::Placed<usize> {
    let devid = function.requester_id();
    let mut explicit = topology::Explicit::None;
    let mut catch_all = topology::CatchAll::None;
    for (at, unit) in units.iter().enumerate() {
        if unit.segment != function.segment() {
            continue;
        }
        if unit.entries.contains(&Entry::All) {
            catch_all = match catch_all {
                topology::CatchAll::None => topology::CatchAll::One(at),
                _ => topology::CatchAll::Several,
            };
        }
        match ivrs::names_among(unit.entries.iter().copied(), devid) {
            Named::Own if claims(false, function, function, bridges) == topology::Behind::Own => {
                if matches!(explicit, topology::Explicit::None) {
                    explicit = topology::Explicit::Own(at);
                }
            }
            Named::Own | Named::Aliased { .. } => explicit = topology::Explicit::Unfollowed,
            Named::No => {}
        }
    }
    topology::place(explicit, catch_all, function, bridges)
}

pub(crate) use ivrs::Exempt;

/// Whether `function`, of `class` and with bus mastering `bus_master` as
/// enumeration found it, is one of D10's two
/// (`ferrix_acpi::ivrs::exempt`, host-tested there).
pub(crate) fn exempt(
    units: &[Placing],
    function: Address,
    class: u32,
    bus_master: bool,
) -> Option<Exempt> {
    ivrs::exempt(
        units
            .iter()
            .map(|unit| (unit.segment, unit.devid, unit.entries.as_slice())),
        function.segment(),
        function.requester_id(),
        class,
        bus_master,
    )
}

/// The functions D10 exempted at stage 10's report: never given a domain
/// or a route.
static EXEMPT: Once<Vec<Address>> = Once::new();

/// Record the exempt functions, once.
pub(crate) fn note_exempt(functions: Vec<Address>) {
    let _ = EXEMPT.call_once(|| functions);
}

/// Whether D10 exempted `function`.
pub(crate) fn is_exempt(function: Address) -> bool {
    EXEMPT
        .get()
        .is_some_and(|exempt| exempt.contains(&function))
}

/// The devid the special entry of a unit gives the I/O APIC `id`.
pub(crate) fn ioapic_source(table: &Ivrs<'_>, id: u8) -> Option<u16> {
    table
        .units()
        .find_map(|unit| unit.special(SPECIAL_IOAPIC, id))
}

/// What a unit's IVMDs ask of its bring-up (§4.1).
#[derive(Debug)]
pub(crate) enum Plan {
    /// None reaches it.
    None,
    /// Each function's identity domain, with its pages and permissions.
    Honour(Vec<(Address, Vec<Identity>, ferrix_paging::MapFlags)>),
    /// It is held to remapping only, for this reason.
    Hold(&'static str),
}

/// Whether `table`'s IVMDs that reach the unit `unit` can be honoured, by
/// the RMRR rules (`ferrix_paging::vtd::rmrr`): whole pages, in reach, over
/// no RAM, no frame the allocator holds, none overlapping another of the
/// function's. An all-peripherals IVMD, one naming a devid this unit does
/// not name as its own, or any that is refused, holds the unit.
pub(crate) fn plan(
    table: &Ivrs<'_>,
    unit: &Placing,
    memory: &[ferrix_bootinfo::MemRegion],
) -> Plan {
    let ram = || {
        memory
            .iter()
            .filter(|region| region.kind.is_ram())
            .map(|region| (region.base, region.len))
    };
    let mut functions: Vec<(Address, Vec<Identity>, ferrix_paging::MapFlags)> = Vec::new();
    for region in table.memory().filter(ivrs::Ivmd::kept) {
        if region.kind == ivrs::IVMD_ALL {
            return Plan::Hold("an IVMD for every peripheral");
        }
        let last = if region.kind == ivrs::IVMD_ONE {
            region.devid
        } else {
            region.aux
        };
        let Some(limit) = region.limit() else {
            return Plan::Hold("an IVMD of no length");
        };
        let Ok(identity) = rmrr::identity(region.start, limit, ram()) else {
            return Plan::Hold("an IVMD the RMRR rules refuse");
        };
        if super::frames_outside_allocator(identity).is_err() {
            return Plan::Hold("an IVMD over a frame the allocator holds");
        }
        let exclusion = region.flags & ivrs::IVMD_EXCLUSION != 0;
        let flags = ferrix_paging::MapFlags {
            read: exclusion || region.flags & ivrs::IVMD_READ != 0,
            write: exclusion || region.flags & ivrs::IVMD_WRITE != 0,
            ..ferrix_paging::MapFlags::DMA
        };
        for devid in region.devid..=last {
            if ivrs::names_among(unit.entries.iter().copied(), devid) != Named::Own {
                return Plan::Hold("an IVMD naming a devid the unit does not name as its own");
            }
            let [bus, devfn] = devid.to_be_bytes();
            let Some(function) = Address::new(unit.segment, bus, devfn >> 3, devfn & 7) else {
                return Plan::Hold("an IVMD naming no PCI address");
            };
            let at = match functions.iter().position(|(named, ..)| *named == function) {
                Some(at) => at,
                None => {
                    // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                    functions.push((function, Vec::new(), flags));
                    functions.len() - 1
                }
            };
            let Some((_, regions, _)) = functions.get_mut(at) else {
                continue;
            };
            // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
            regions.push(identity);
            if rmrr::disjoint(regions).is_err() {
                return Plan::Hold("two IVMDs of one function overlap");
            }
        }
    }
    if functions.is_empty() {
        Plan::None
    } else {
        Plan::Honour(functions)
    }
}

/// Bring every unit of `table` up, and record each with its placing, its
/// identity domains and its interrupt state in `programmed`.
///
/// A unit an IVMD holds is brought up remapping only; a unit that would
/// not come up is left alone, and placement says so (`None` index).
pub(crate) fn bring_up(
    table: &Ivrs<'_>,
    memory: &[ferrix_bootinfo::MemRegion],
    switched_off: bool,
    programmed: &mut Programmed,
    report: &mut super::BringUp,
) {
    let Some(mut units) = placing(table) else {
        println!(
            "  iommu    the IVRS cannot be read whole: no AMD-Vi unit is brought up, and every \
             PCI function is unresolved"
        );
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        super::remapping::record(alloc::vec![ferrix_paging::vtd::remap::UnitState {
            remapping: false,
            compatibility_blocked: false,
        }]);
        return;
    };
    let mut states = Vec::new();
    for (unit, ivhd) in units.iter_mut().zip(table.units()) {
        let mut state = ferrix_paging::vtd::remap::UnitState {
            remapping: false,
            compatibility_blocked: false,
        };
        let plan = plan(table, unit, memory);
        let opened = if switched_off {
            Err("ferrix.amdvi=off asked for every unit to be left alone")
        } else {
            amdvi::Unit::open(&ivhd, matches!(plan, Plan::Hold(_)))
        };
        match opened.and_then(|opened| {
            let identities = attach(&opened, &plan)?;
            state = super::remapping::bring_up_amdvi(&opened, table, &ivhd)?;
            Ok((opened, identities))
        }) {
            Ok((opened, identities)) => {
                say(&opened, &plan, state);
                let index = programmed.amdvi.len();
                for identity in identities {
                    println!(
                        "  iommu    amd-vi unit {:#x}: IVMD identity-mapped for pci {}, held for \
                         the boot and never detached",
                        opened.phys(),
                        identity.function
                    );
                    // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                    programmed.amdvi_identity.push((index, identity));
                }
                if opened.translating() {
                    report.amdvi += 1;
                } else {
                    report.remapping_only += 1;
                }
                // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
                programmed.amdvi.push(opened);
                unit.index = Some(index);
            }
            Err(why) => {
                println!("  iommu    amd-vi unit {:#x}: left alone: {why}", unit.base);
                report.refused += 1;
                report.why = Some(why);
            }
        }
        // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
        states.push(state);
    }
    if switched_off {
        println!(
            "  iommu    ferrix.amdvi=off: every AMD-Vi unit is left alone, so no DMA is \
             translated and no interrupt remapped; this is a recovery configuration, not an \
             evaluated one"
        );
    }
    super::remapping::record(states);
    programmed.amdvi_places = units;
}

/// Attach every identity domain `plan` honours on `unit`, before it is
/// enabled.
fn attach(unit: &amdvi::Unit, plan: &Plan) -> Result<Vec<IdentityDomain>, &'static str> {
    let mut made = Vec::new();
    if let Plan::Honour(functions) = plan {
        for (function, regions, flags) in functions {
            let attached = unit.attach_identity(*function, regions, *flags)?;
            // FATAL-ALLOC: boot only: IOMMU units are found, placed and programmed once, before any program runs.
            made.push(IdentityDomain::new_amdvi(
                *function,
                attached,
                regions.clone(),
            ));
        }
    }
    Ok(made)
}

/// A unit's bring-up line.
fn say(unit: &amdvi::Unit, plan: &Plan, state: ferrix_paging::vtd::remap::UnitState) {
    let remap = if state.remapping {
        "remapping on (512 entries per function, 32-bit format)"
    } else {
        "remapping off"
    };
    let (base, limit) = unit.exclusion();
    let exclusion = if base == 0 && limit == 0 {
        "exclusion range off"
    } else {
        "exclusion range NOT off"
    };
    match plan {
        Plan::Hold(why) => println!(
            "  iommu    amd-vi unit {:#x} (devid {:#06x}): commands and event log on, {remap}, \
             {exclusion}, translation left off: {why}; the DMA of every function behind this \
             unit is untranslated and reaches all of memory",
            unit.phys(),
            unit.devid(),
        ),
        Plan::None | Plan::Honour(_) => println!(
            "  iommu    amd-vi unit {:#x} (devid {:#06x}): translating (three-level), commands \
             and event log on, {remap}, {exclusion}; 65536 device table entries blocked by \
             default",
            unit.phys(),
            unit.devid(),
        ),
    }
}
