//! Whether the kernel drives a VT-d unit, read from its version, capability
//! and extended capability registers alone, and what it drives it with: the
//! walk, the fault records, the drain bits.
//!
//! Reference: Intel Virtualization Technology for Directed I/O, Architecture
//! Specification, "Register Descriptions" (10.4); read against revisions 1
//! (QEMU's unit, `VER` 1.0) and 4 and 5 (the N150's `dmar0` and `dmar1`).
//!
//! # Which versions
//!
//! The kernel drives a unit in legacy mode only: `RTADDR` with `TTM` 00,
//! `GCMD`/`GSTS`, the invalidation queue's registers, `IRTA`, `FSTS`, the
//! fault recording registers at `CAP.FRO` and the IOTLB register at
//! `ECAP.IRO` (read, never written). Their layout and meaning are the same
//! in every revision from 1 to 5, and each feature that differs between
//! revisions has a capability bit of its own, which [`accept`] reads. A
//! major version other than 1, 4 or 5 is refused only because no unit of
//! it has been read against this driver: an evidence rule, not a
//! limitation found. `DT`, `PT`, `SC`, `EIM` and `SMTS` stay unused.
//!
//! # Which walk
//!
//! The widest the unit offers, as Linux's `iommu_calculate_agaw` picks:
//! four levels when `SAGAW` bit 2 is set, three when only bit 1 is. A
//! domain still reaches only 39 bits whichever walk it has, so a unit whose
//! guest address width is below 39 is refused.

/// The window of registers the kernel maps: every register legacy mode uses
/// is in the first page.
pub const WINDOW: u64 = 0x1000;

/// The bits of I/O address a domain reaches, whatever its walk.
pub const DOMAIN_BITS: u32 = 39;

/// CAP: the unit needs its write buffer flushed after every change.
const CAP_RWBF: u64 = 1 << 4;
/// CAP: protected low-memory region support.
const CAP_PLMR: u64 = 1 << 5;
/// CAP: protected high-memory region support.
const CAP_PHMR: u64 = 1 << 6;
/// CAP: caching mode.
const CAP_CM: u64 = 1 << 7;
/// CAP: the `SAGAW` bit for three-level, 39-bit tables.
const CAP_SAGAW_39: u64 = 1 << 9;
/// CAP: the `SAGAW` bit for four-level, 48-bit tables.
const CAP_SAGAW_48: u64 = 1 << 10;
/// CAP: `SLLPS` bit 0, second-level 2 MiB pages.
const CAP_SLLPS_2M: u64 = 1 << 34;
/// CAP: write draining.
const CAP_DWD: u64 = 1 << 54;
/// CAP: read draining.
const CAP_DRD: u64 = 1 << 55;

/// ECAP: page-walk coherency.
const ECAP_C: u64 = 1 << 0;
/// ECAP: queued invalidation.
const ECAP_QI: u64 = 1 << 1;
/// ECAP: interrupt remapping.
const ECAP_IR: u64 = 1 << 3;

/// The major versions the driver has been read against.
pub const VERSIONS: [u32; 3] = [1, 4, 5];

/// The walk a unit's domains get.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Walk {
    /// Three levels over 39 bits (`ferrix_paging::vtd::VtdSecondLevel`).
    Three,
    /// Four levels over 48 bits (`ferrix_paging::vtd::VtdFourLevel`).
    Four,
}

impl Walk {
    /// The context entry's address width field for this walk: 1 for 39
    /// bits, 2 for 48.
    #[must_use]
    pub const fn aw(self) -> u64 {
        match self {
            Walk::Three => 1,
            Walk::Four => 2,
        }
    }

    /// How the console names it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Walk::Three => "three-level",
            Walk::Four => "four-level",
        }
    }
}

/// Which drain bits an IOTLB invalidation sets.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Drain {
    /// `DR`: reads using the old translation complete first.
    pub read: bool,
    /// `DW`: writes using the old translation complete first.
    pub write: bool,
}

/// What the kernel drives an accepted unit with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Accepted {
    /// The walk its domains get.
    pub walk: Walk,
    /// Its guest address width in bits: `CAP.MGAW` plus one.
    pub guest_bits: u32,
    /// Offset of its first fault recording register.
    pub faults: u64,
    /// How many fault recording registers it has: `CAP.NFR` plus one.
    pub records: u32,
    /// Offset of its IOTLB invalidation register.
    pub iotlb: u64,
    /// The drain bits its IOTLB invalidations set.
    pub drain: Drain,
    /// Caching mode.
    pub caching: bool,
    /// Whether its walk snoops the caches.
    pub coherent: bool,
    /// Whether it can remap interrupts (`ECAP.IR`).
    pub remaps: bool,
    /// Whether it has protected memory regions, so `PMEN` exists.
    pub protected_regions: bool,
    /// Whether its walk takes 2 MiB second-level pages (`CAP.SLLPS` bit 0),
    /// so a domain may map a pin's aligned runs as blocks (`L.iommu.105`).
    pub blocks: bool,
    /// How many domain identifiers it supports.
    pub identifiers: u32,
}

/// The major version in a `VER` value.
#[must_use]
pub const fn major(ver: u32) -> u32 {
    (ver >> 4) & 0xF
}

/// The minor version in a `VER` value.
#[must_use]
pub const fn minor(ver: u32) -> u32 {
    ver & 0xF
}

/// Whether the kernel drives the unit whose registers read `ver`, `cap` and
/// `ecap`, and with what.
///
/// # Errors
///
/// Why the unit is left alone, as the console says it.
pub fn accept(ver: u32, cap: u64, ecap: u64) -> Result<Accepted, &'static str> {
    if !VERSIONS.contains(&major(ver)) {
        return Err("it is of a version this driver was not read against (it reads 1, 4 and 5)");
    }
    let walk = if cap & CAP_SAGAW_48 != 0 {
        Walk::Four
    } else if cap & CAP_SAGAW_39 != 0 {
        Walk::Three
    } else {
        return Err("it walks neither three- nor four-level tables");
    };
    let guest_bits = ((cap >> 16) & 0x3F) as u32 + 1;
    if guest_bits < DOMAIN_BITS {
        return Err("its guest address width is below the 39 bits a domain reaches");
    }
    if cap & CAP_RWBF != 0 {
        return Err("it needs its write buffer flushed");
    }
    if ecap & ECAP_QI == 0 {
        return Err("it has no invalidation queue");
    }
    let faults = ((cap >> 24) & 0x3FF) * 16;
    let records = ((cap >> 40) & 0xFF) as u32 + 1;
    let iotlb = ((ecap >> 8) & 0x3FF) * 16 + 8;
    if faults + u64::from(records) * 16 > WINDOW || iotlb + 8 > WINDOW {
        return Err("its fault records or IOTLB register lie outside its first page");
    }
    Ok(Accepted {
        walk,
        guest_bits,
        faults,
        records,
        iotlb,
        drain: Drain {
            read: cap & CAP_DRD != 0,
            write: cap & CAP_DWD != 0,
        },
        caching: cap & CAP_CM != 0,
        coherent: ecap & ECAP_C != 0,
        remaps: ecap & ECAP_IR != 0,
        protected_regions: cap & (CAP_PLMR | CAP_PHMR) != 0,
        blocks: cap & CAP_SLLPS_2M != 0,
        identifiers: 1 << (4 + 2 * (cap & 0b111)),
    })
}

/// The fault recording registers in the order they are read: from the one
/// `FSTS.FRI` (bits 15:8 of `fsts`) names, round all `records` of them, as
/// the unit fills them and Linux's `dmar_fault` reads them.
pub fn fault_records(fsts: u32, records: u32) -> impl Iterator<Item = u32> {
    let records = records.max(1);
    let first = ((fsts >> 8) & 0xFF) % records;
    (0..records).map(move |step| (first + step) % records)
}

/// What stage 10's discovery line counts of the functions placed behind a
/// unit: all of them, and apart those whose unit does not translate -- one
/// left alone, or one an RMRR holds to remapping only -- whose DMA reaches
/// all of memory and is never counted as translated.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Tally {
    /// Functions placed behind a unit.
    pub behind: usize,
    /// Of them, those whose unit does not translate.
    pub untranslated: usize,
}

impl Tally {
    /// Count one function placed behind a unit that translates or not.
    pub fn add(&mut self, unit_translates: bool) {
        self.behind += 1;
        if !unit_translates {
            self.untranslated += 1;
        }
    }
}

/// Whether the end-of-boot fault audit reads a programmed unit's records:
/// every unit that translates or remaps, since one that only remaps still
/// records the interrupt requests it refuses.
#[must_use]
pub const fn audited(translates: bool, remaps: bool) -> bool {
    translates || remaps
}

/// The command-line option that leaves every VT-d unit alone.
pub const OFF_OPTION: &str = "ferrix.vtd";

/// Whether the command line's `ferrix.vtd` value, if it has one, leaves
/// every VT-d unit alone: `off` exactly, and nothing else. A recovery
/// configuration for a machine whose units break its boot, never an
/// evaluated one: no DMA is translated and no interrupt remapped.
#[must_use]
pub fn switched_off(value: Option<&str>) -> bool {
    value == Some("off")
}
