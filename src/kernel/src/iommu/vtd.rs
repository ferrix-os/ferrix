//! Intel VT-d in legacy mode: a remapping unit's root and context tables, and
//! the second-level tables of the domains built on it.
//!
//! Written against the specification and checked against QEMU's
//! `hw/i386/intel_iommu.c`, which the boot test runs. Legacy mode needs, and
//! this does:
//!
//! * a **root table**, one entry per bus, pointing at that bus's **context
//!   table**, one entry per device and function, which names a domain and the
//!   root of its second-level tables — `src/lib/kernel/paging`'s
//!   [`VtdSecondLevel`], three levels over 39 bits, or [`VtdFourLevel`],
//!   four over 48, whichever is the widest the unit walks
//!   (`ferrix_paging::vtd::unit::accept`);
//! * **queued invalidation** of the context cache and the IOTLB, after
//!   anything the unit may have cached changes (below);
//! * **translation on**, after which a function with no context entry reaches
//!   nothing at all.
//!
//! # A unit whose walk does not snoop
//!
//! A unit with `ECAP.C` clear reads its root, context and second-level
//! entries from memory, past every processor's cache, and this kernel writes
//! them through the cached direct map. So on such a unit every entry written
//! is noted in an [`Unpublished`] record and cleaned to memory --
//! `clflush`, then `mfence` ([`crate::arch::clean_for_walker`]) -- before
//! the invalidation that publishes it, and a fresh table is cleaned whole
//! before anything links it ([`table`]). Each change checks its record is
//! empty at its own publish point -- the invalidation that publishes an
//! attach or a detach, the return of a map or an unmap -- and is refused if
//! it is not, so a write not cleaned is never published; what the checks
//! find is counted for stage 10's check (finding F-58). QEMU's unit reports
//! `C` clear, though it walks coherently, so every boot test runs this
//! path. A unit with `C` set is written as before, with nothing noted or
//! cleaned.
//!
//! What the check proves is that each change's noted writes were cleaned
//! before its own publish point. That every write to memory the unit walks
//! is noted rests on construction: [`write_entry`], [`table`] and the
//! mapper's writes through `ferrix_paging::coherence::Walked` (in
//! `mm::map_io` and `mm::unmap_io`) are this file's only writers of that
//! memory, and no other direct-map write is in it. `enable` and `flush`
//! write nothing, so their invalidations check no record: `enable` relies
//! on `open`'s clean of the root table, `flush` on `map`'s and `unmap`'s
//! own checks.
//!
//! # Queued invalidation, and only that
//!
//! Every invalidation goes through the unit's invalidation queue
//! (`docs/NVIDIA.md` §12.3, N0g): one descriptor and a fenced wait
//! descriptor behind it, whose status write -- a sequence number of the
//! unit's own, so a late completion of an earlier wait is never taken for
//! this one -- is what the kernel waits for, up to [`PATIENCE_NANOS`]. A
//! unit without `ECAP.QI` is refused, so there is no register-based
//! invalidation left to issue while `QIE` is set, which the specification
//! forbids. [`Unit::invalidate_context`] and [`Unit::invalidate_iotlb`] are
//! the only two entry points, so every caller -- an attach, a detach, a
//! flush after an unmap or a caching-mode map, the quarantine's release --
//! goes through the queue without a change at its call site.
//!
//! **A failed invalidation** is a wait whose status is not written within
//! the unit's patience, or `FSTS.IQE`, `ICE` or `ITE` seen while waiting.
//! It answers `Err`, and every caller already treats that as "the unit may
//! still reach it": an unpin hands the pin back and its frames are kept, a
//! detach keeps the root table. After `IQE` or `ITE` the unit has stopped
//! fetching descriptors, so it is marked **failed**: every later
//! invalidation on it fails at once without touching the queue, nothing is
//! released from its domains again until reboot, and the end-of-boot audit
//! counts the failure as a stray fault (FX-1007). There is no recovery: a
//! wedged queue is a broken unit, and holding memory is the safe side.
//!
//! Fault events are masked (`FECTL.IM`), and never used.
//!
//! # Which units
//!
//! `ferrix_paging::vtd::unit::accept` decides from `VER`, `CAP` and
//! `ECAP` alone: a major version of 1, 4 or 5 -- QEMU's unit and the
//! N150's two, whose legacy-mode register interface (revisions 1 to 5 of
//! the specification) is all this driver uses -- a three- or four-level
//! walk, a guest address width of at least the 39 bits a domain reaches, no
//! write buffer to flush, an invalidation queue, and its fault records and
//! IOTLB register inside the first page. `DT`, `PT`, `SC`, `EIM` and
//! `SMTS` stay unused: no device TLB, no pass-through, no snoop control, the
//! xAPIC table format, legacy mode only.
//!
//! A unit with protected memory regions (`CAP.PLMR` or `PHMR`) has them
//! turned off once it translates, if firmware left them on, as Linux's
//! `iommu_disable_protect_mem_regions` does: otherwise DMA into them stays
//! blocked whatever a domain maps.

use alloc::collections::{BTreeMap, BTreeSet};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::coherence::Unpublished;
use ferrix_paging::vtd::queue::{
    self, Completion, ContextScope, Descriptor, FSTS_ICE, FSTS_IQE, FSTS_QUEUE_ERRORS,
    InterruptEntryScope, IotlbScope, Outcome,
};
use ferrix_paging::vtd::remap::{self, Remap};
use ferrix_paging::vtd::unit::{self as policy, Drain, Walk};
use ferrix_paging::vtd::{VtdFourLevel, VtdSecondLevel};
use ferrix_paging::{MapError, MapFlags, PhysAddr};
use ferrix_pci::Address;
use ferrix_sync::IrqSpinLock;

use super::gate::{self, Gate};
use super::{Cause, Fault, Wait};
use crate::mmio::Mmio;
use crate::{arch, mm, timer, vmap};

/// Bytes of registers mapped: every register legacy mode uses is in the first
/// page.
const WINDOW: u64 = policy::WINDOW;

/// Version register.
const VER: u64 = 0x00;
/// Capability register.
const CAP: u64 = 0x08;
/// Extended capability register.
const ECAP: u64 = 0x10;
/// Global command register.
const GCMD: u64 = 0x18;
/// Global status register.
const GSTS: u64 = 0x1C;
/// Root table address register.
const RTADDR: u64 = 0x20;
/// Context command register: read only, to see that no register-based
/// invalidation was ever issued ([`Unit::register_invalidation_pending`]).
const CCMD: u64 = 0x28;
/// Fault status register.
const FSTS: u64 = 0x34;
/// Fault event control register: bit 31 masks the unit's fault event.
const FECTL: u64 = 0x38;
/// Protected memory enable register: `EPM` bit 31, `PRS` bit 0. Exists
/// only on a unit with `CAP.PLMR` or `PHMR`.
const PMEN: u64 = 0x64;
/// Invalidation queue head: the next descriptor the unit fetches.
const IQH: u64 = 0x80;
/// Invalidation queue tail: the slot after the last descriptor written.
const IQT: u64 = 0x88;
/// Invalidation queue address, size and descriptor width.
const IQA: u64 = 0x90;
/// Invalidation event control: bit 31 masks the completion event.
const IECTL: u64 = 0xA0;

/// GCMD: turn translation on; GSTS: it is on.
const TE: u32 = 1 << 31;
/// GCMD: take the root table pointer; GSTS: it is taken.
const SRTP: u32 = 1 << 30;
/// GCMD: turn the invalidation queue on; GSTS (`QIES`): it is on.
const QIE: u32 = 1 << 26;
/// GCMD: turn interrupt remapping on; GSTS (`IRES`): it is on.
const IRE: u32 = 1 << 25;
/// GCMD: take the interrupt remapping table pointer; GSTS (`IRTPS`): it
/// is taken.
const SIRTP: u32 = 1 << 24;
/// GSTS (`CFIS`): compatibility-format interrupts pass through untranslated.
/// Read back clear after `IRE`, or remapping is turned off again.
const CFIS: u32 = 1 << 23;
/// Interrupt remapping table address register.
const IRTA: u64 = 0xB8;
/// GSTS bits reporting a standing enable, which every GCMD write repeats so as
/// not to turn it off: translation, queued invalidation and interrupt
/// remapping. Not `CFI` (bit 23): a compatibility-format permission
/// firmware left set is cleared by the kernel's first GCMD write rather than
/// carried into every later one.
const STANDING: u32 = TE | QIE | IRE;

/// FECTL: the fault event is masked. Set in `open`: firmware may leave a
/// fault event's message armed, and a unit's own messages are never
/// remapped.
const FECTL_MASKED: u32 = 1 << 31;
/// PMEN: enable protected memory regions.
const PMEN_EPM: u32 = 1 << 31;
/// PMEN: protected region status, set while they are enforced.
const PMEN_PRS: u32 = 1 << 0;

/// IECTL: the invalidation completion event is masked. Set before the queue
/// goes on: the event's message registers are never programmed, and the only
/// wait that raises it is check R7's.
const IECTL_MASKED: u32 = 1 << 31;

/// CCMD: a register-based context-cache invalidation is in progress.
const ICC: u64 = 1 << 63;
/// IOTLB register: a register-based IOTLB invalidation is in progress.
const IVT: u64 = 1 << 63;

/// FSTS: primary fault overflow, a fault was lost for want of a free record.
/// Write one to clear. While it is set the unit records nothing: QEMU's
/// `vtd_report_frcd_fault` drops every fault until it is cleared.
const PFO: u32 = 1 << 0;
/// Fault recording register, the top 32 bits of its high quad: the record
/// holds a fault.
///
/// Kept as a 32-bit mask because this bit must be read by itself, before the
/// rest of the record. See [`Unit::take_fault`].
const FRCD_F: u32 = 1 << 31;
/// The same word: the faulting access was a read.
const FRCD_READ: u32 = 1 << 30;

/// Root and context entries: present.
const PRESENT: u64 = 1;

/// How long a command may take before the unit is given up on.
const PATIENCE_NANOS: u64 = 100_000_000;

/// How long check R7's wait, which writes no status and so can never be seen
/// to complete, is waited for before it fails: the same timeout path as
/// [`PATIENCE_NANOS`]'s, without spending a tenth of a second of every boot
/// on an answer known in advance (the test-time priority).
const UNWRITTEN_PATIENCE_NANOS: u64 = 2_000_000;

/// Why a publish was refused: a table write not cleaned to memory.
const UNCLEANED_WHY: &str = "a table write was not cleaned to memory before it was published";

/// Units opened whose walk does not snoop.
static UNITS_CLEANING: AtomicU64 = AtomicU64::new(0);
/// Entry writes cleaned to memory on those units.
static ENTRIES_CLEANED: AtomicU64 = AtomicU64::new(0);
/// Fresh tables cleaned whole on those units.
static TABLES_CLEANED: AtomicU64 = AtomicU64::new(0);
/// Changes whose record was checked at their own publish point: each
/// attach, detach, map and unmap.
static CHANGES_CHECKED: AtomicU64 = AtomicU64::new(0);
/// Changes whose check found a write not cleaned, and were refused.
static UNCLEANED: AtomicU64 = AtomicU64::new(0);

/// Context-cache invalidations queued and waited for.
static QUEUED_CONTEXT: AtomicU64 = AtomicU64::new(0);
/// IOTLB invalidations queued and waited for.
static QUEUED_IOTLB: AtomicU64 = AtomicU64::new(0);
/// Invalidations that failed: a wait not completed, or a queue error.
static INVALIDATIONS_FAILED: AtomicU64 = AtomicU64::new(0);
/// Units marked failed after their queue stopped.
static UNITS_FAILED: AtomicU64 = AtomicU64::new(0);
/// Invalidation completion errors taken and cleared.
static COMPLETION_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Interrupt entry cache invalidations queued and waited for.
static QUEUED_INTERRUPT_ENTRY: AtomicU64 = AtomicU64::new(0);

/// Why an invalidation on a failed unit is refused at once.
const FAILED_WHY: &str = "its invalidation queue stopped at a bad descriptor, so it takes no \
                          invalidation again";

/// What the units' invalidation queues did, for stage 10's check R6.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Invalidations {
    /// Context-cache invalidations queued, each waited for.
    pub(crate) context: u64,
    /// IOTLB invalidations queued, each waited for.
    pub(crate) iotlb: u64,
    /// Interrupt entry cache invalidations queued, each waited for.
    pub(crate) interrupt_entry: u64,
    /// Invalidations that failed.
    pub(crate) failed: u64,
    /// Units marked failed.
    pub(crate) units_failed: u64,
    /// Completion errors (`ICE`) taken and cleared, each a failed
    /// invalidation.
    pub(crate) completion_errors: u64,
}

/// What every unit's invalidation queue has done since boot.
pub(crate) fn invalidations() -> Invalidations {
    Invalidations {
        context: QUEUED_CONTEXT.load(Ordering::Relaxed),
        iotlb: QUEUED_IOTLB.load(Ordering::Relaxed),
        interrupt_entry: QUEUED_INTERRUPT_ENTRY.load(Ordering::Relaxed),
        failed: INVALIDATIONS_FAILED.load(Ordering::Relaxed),
        units_failed: UNITS_FAILED.load(Ordering::Relaxed),
        completion_errors: COMPLETION_ERRORS.load(Ordering::Relaxed),
    }
}

/// What the units' cleaning did, for stage 10's check.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cleaning {
    /// Units whose walk does not snoop.
    pub(crate) units: u64,
    /// Entry writes cleaned to memory.
    pub(crate) entries: u64,
    /// Fresh tables cleaned whole.
    pub(crate) tables: u64,
    /// Changes checked at their own publish point.
    pub(crate) checked: u64,
    /// Changes whose check found a write not cleaned.
    pub(crate) uncleaned: u64,
}

/// What every unit's cleaning has done since boot.
pub(crate) fn cleaning() -> Cleaning {
    Cleaning {
        units: UNITS_CLEANING.load(Ordering::Relaxed),
        entries: ENTRIES_CLEANED.load(Ordering::Relaxed),
        tables: TABLES_CLEANED.load(Ordering::Relaxed),
        checked: CHANGES_CHECKED.load(Ordering::Relaxed),
        uncleaned: UNCLEANED.load(Ordering::Relaxed),
    }
}

/// One remapping unit.
#[derive(Debug)]
pub(crate) struct Unit {
    /// Physical address of its registers, which names it on the console.
    phys: u64,
    /// Its registers.
    registers: Mmio,
    /// Physical address of the root table.
    root: u64,
    /// Physical address of its invalidation queue: one frame of
    /// [`queue::QUEUE_LENGTH`] descriptors, which only this unit's code
    /// writes, under `commands`.
    queue: u64,
    /// Physical address of the frame whose first word each wait descriptor's
    /// status write lands in.
    status: u64,
    /// The queue slot the next descriptor goes in. Changed only under
    /// `commands`.
    tail: AtomicU32,
    /// The last wait's status data. Changed only under `commands`.
    sequence: AtomicU32,
    /// Whether the queue stopped (`IQE` or `ITE`): no invalidation is made on
    /// the unit again.
    failed: AtomicBool,
    /// Whether the end-of-boot audit has been told of the failure.
    failure_reported: AtomicBool,
    /// Queue errors a boot check planted, read as if `FSTS` held them and
    /// cleared as `FSTS`'s are: only `check_planted_completion_error` sets
    /// any. QEMU's unit never raises `ICE` and ignores a write to it, so
    /// the clearing path would otherwise never run.
    planted: AtomicU32,
    /// Physical address of its interrupt remapping table, one frame of
    /// [`remap::TABLE_ENTRIES`], or 0 on a unit that cannot remap: the DMAR
    /// does not say `INTR_REMAP`, or `ECAP.IR` is clear.
    irt: u64,
    /// Whether `IRE` read back set and `CFIS` clear after it: the unit
    /// remaps every interrupt request and blocks compatibility format.
    remapping: AtomicBool,
    /// Offset of the first fault recording register.
    faults: u64,
    /// How many fault recording registers it has.
    records: u32,
    /// The walk its domains get, and the context entries' address width.
    walk: Walk,
    /// The drain bits its IOTLB invalidations set.
    drain: Drain,
    /// Whether it has protected memory regions, so `PMEN` may be read.
    protected_regions: bool,
    /// Whether translation is on: false for a unit an RMRR holds, which
    /// only remaps (`iommu::bring_up_vtd`).
    translating: AtomicBool,
    /// Offset of the IOTLB invalidation register.
    iotlb: u64,
    /// Whether the unit is in caching mode.
    caching: bool,
    /// Whether the unit's walk snoops the caches (`ECAP.C`). If not, every
    /// table write is cleaned to memory before it is published.
    coherent: bool,
    /// How many domain identifiers the unit supports.
    identifiers: u32,
    /// The tables' bookkeeping.
    tables: IrqSpinLock<Tables, arch::Irq>,
    /// The stream and page of the record [`Unit::take_fault`] last took, which
    /// an overflow is reported against. Held across each take, so two cannot
    /// interleave and this is always the last record taken.
    taken: IrqSpinLock<Option<(u32, u64)>, arch::Irq>,
    /// Held across a command and its wait, so two cannot interleave. A gate,
    /// not a lock: the wait is made with interrupts on.
    commands: Gate,
}

/// What a unit has handed out.
#[derive(Debug, Default)]
struct Tables {
    /// Each bus's context table, by physical address.
    contexts: BTreeMap<u8, u64>,
    /// Domain identifiers in use.
    identifiers: BTreeSet<u16>,
}

/// What became of a unit's protected memory regions once it translated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Protected {
    /// The unit has none (`CAP.PLMR` and `PHMR` clear): `PMEN` not read.
    None,
    /// Firmware left them off.
    OffAsFound,
    /// Firmware left them on, and the kernel turned them off.
    TurnedOff,
}

impl Protected {
    /// What the unit's line says.
    pub(crate) const fn says(self) -> &'static str {
        match self {
            Protected::None => "no PMR",
            Protected::OffAsFound => "PMR off as found",
            Protected::TurnedOff => "PMR turned off",
        }
    }
}

/// A domain on one unit: the function whose context entry names it, the
/// identifier it names, and the root of its second-level tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Attached {
    /// The function.
    function: Address,
    /// The domain identifier.
    identifier: u16,
    /// Physical address of the second-level root table.
    root: u64,
}

impl Attached {
    /// The source ID the unit sees the function as.
    pub(crate) fn stream(&self) -> u32 {
        u32::from(self.function.requester_id())
    }
}

impl Unit {
    /// Map the unit whose registers are at `phys`, and require it to be able to
    /// do what this driver asks (`ferrix_paging::vtd::unit::accept`): a
    /// version read against, a three- or four-level walk, no write buffer
    /// to flush, an invalidation queue, its records in its first page; and
    /// not left translating. A unit firmware left remapping interrupts, or
    /// with its queue on, is turned off first and must read back off. Its
    /// fault event is masked.
    ///
    /// # Errors
    ///
    /// Why the unit is left alone; the unit's registers are printed beside
    /// it.
    ///
    /// `intr_remap` is the DMAR's `INTR_REMAP` flag: with it and `ECAP.IR`
    /// the unit is given an interrupt remapping table, and may remap.
    pub(crate) fn open(phys: u64, intr_remap: bool) -> Result<Unit, &'static str> {
        let base =
            vmap::map_device(phys, WINDOW).map_err(|_| "its registers could not be mapped")?;
        let registers = Mmio::at(base);
        let refuse = |why: &'static str| -> Result<Unit, &'static str> {
            let _ = vmap::unmap_device(base);
            Err(why)
        };
        let ver = registers.read32(VER);
        let cap = read64(registers, CAP);
        let ecap = read64(registers, ECAP);
        let accepted = match policy::accept(ver, cap, ecap) {
            Ok(accepted) => accepted,
            Err(why) => return refuse(say_refused(phys, ver, cap, ecap, why)),
        };
        if registers.read32(GSTS) & TE != 0 {
            return refuse("firmware left it translating");
        }
        if let Err(why) = stop_firmware(registers) {
            return refuse(why);
        }
        registers.write32(FECTL, FECTL_MASKED);
        let coherent = accepted.coherent;
        let mut writes = Unpublished::new(coherent);
        let remaps = intr_remap && accepted.remaps;
        let Some([root, queue, status, irt]) = unit_frames(&mut writes, remaps) else {
            return refuse("no frames for its tables");
        };
        settle(&writes);
        if !coherent {
            let _ = UNITS_CLEANING.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Unit {
            phys,
            registers,
            root,
            queue,
            status,
            tail: AtomicU32::new(0),
            sequence: AtomicU32::new(0),
            failed: AtomicBool::new(false),
            failure_reported: AtomicBool::new(false),
            planted: AtomicU32::new(0),
            irt,
            remapping: AtomicBool::new(false),
            faults: accepted.faults,
            records: accepted.records,
            walk: accepted.walk,
            drain: accepted.drain,
            protected_regions: accepted.protected_regions,
            translating: AtomicBool::new(false),
            iotlb: accepted.iotlb,
            caching: accepted.caching,
            coherent,
            identifiers: accepted.identifiers,
            tables: IrqSpinLock::new(Tables::default()),
            taken: IrqSpinLock::new(None),
            commands: Gate::new(),
        })
    }

    /// The offset of fault recording register `record`.
    fn record(&self, record: u32) -> u64 {
        self.faults + u64::from(record) * 16
    }

    /// The walk its domains get.
    pub(crate) fn walk(&self) -> Walk {
        self.walk
    }

    /// Whether translation is on.
    pub(crate) fn translating(&self) -> bool {
        self.translating.load(Ordering::Acquire)
    }

    /// Whether its fault event reads back masked, as `open` left it
    /// (`L.iommu.70`).
    pub(crate) fn fault_events_masked(&self) -> bool {
        self.registers.read32(FECTL) & FECTL_MASKED != 0
    }

    /// Point the unit at its root table, turn its invalidation queue on, make
    /// it forget what it cached, and turn translation on. From here a
    /// function with no context entry reaches nothing.
    ///
    /// # Errors
    ///
    /// The command the unit never finished.
    pub(crate) fn enable(&self) -> Result<(), &'static str> {
        write64(self.registers, RTADDR, self.root);
        self.command(SRTP, "it never took its root table")?;
        self.start_queue()?;
        // The root table was cleaned when `open` made it, and nothing has
        // been written since: no record to check.
        self.invalidate_context(ContextScope::Global, None)?;
        self.invalidate_iotlb(IotlbScope::Global, None, Wait::Status)?;
        if self.irt != 0 {
            self.start_table()?;
        }
        // A fault firmware left recorded would otherwise be read as ours.
        for record in 0..self.records {
            let flags = self.record(record) + 12;
            if self.registers.read32(flags) & FRCD_F != 0 {
                self.registers.write32(flags, FRCD_F);
            }
        }
        self.registers.write32(FSTS, PFO);
        *self.taken.lock() = None;
        Ok(())
    }

    /// Turn translation on, after [`Unit::enable`] and, on a unit that
    /// remaps, after [`Unit::start_remapping`]. From here a function with
    /// no context entry reaches nothing. Then, on a unit with protected
    /// memory regions firmware left on, turn them off: what the answer says.
    ///
    /// # Errors
    ///
    /// The unit never started translating, or its protected regions would
    /// not turn off -- the unit is then failed, translating still, so DMA
    /// stays confined and nothing it may reach is released.
    pub(crate) fn translate(&self) -> Result<Protected, &'static str> {
        self.command(TE, "it never started translating")?;
        self.translating.store(true, Ordering::Release);
        if !self.protected_regions {
            return Ok(Protected::None);
        }
        if self.registers.read32(PMEN) & PMEN_PRS == 0 {
            return Ok(Protected::OffAsFound);
        }
        // EPM cleared, as Linux's `iommu_disable_protect_mem_regions` does
        // once translation is on: the domains decide from here.
        self.registers
            .write32(PMEN, self.registers.read32(PMEN) & !PMEN_EPM);
        if wait_for(
            || self.registers.read32(PMEN) & PMEN_PRS == 0,
            "its protected memory regions would not turn off",
        )
        .is_err()
        {
            if !self.failed.swap(true, Ordering::AcqRel) {
                let _ = UNITS_FAILED.fetch_add(1, Ordering::Relaxed);
            }
            crate::println!(
                "  iommu    vt-d unit {:#x}: its protected memory regions would not turn \
                 off: the unit is failed, still translating, and nothing it may reach is \
                 released until reboot",
                self.phys
            );
            return Err("its protected memory regions would not turn off");
        }
        Ok(Protected::TurnedOff)
    }

    /// Point the unit at its interrupt remapping table -- `EIME` = 0, 256
    /// entries, every one not present -- and make it forget any entry it
    /// cached. Remapping itself stays off until [`Unit::start_remapping`].
    fn start_table(&self) -> Result<(), &'static str> {
        write64(self.registers, IRTA, remap::table_address(self.irt));
        self.command(SIRTP, "it never took its interrupt remapping table")?;
        self.invalidate_interrupt_entry(InterruptEntryScope::Global)
    }

    /// Physical address of its registers, which names it on the console.
    pub(crate) fn phys(&self) -> u64 {
        self.phys
    }

    /// Whether the unit has an interrupt remapping table: the DMAR says
    /// `INTR_REMAP` and the unit `ECAP.IR`.
    pub(crate) fn can_remap(&self) -> bool {
        self.irt != 0
    }

    /// Whether the unit remaps every interrupt request and blocks
    /// compatibility format: `IRE` read back set and `CFIS` clear.
    pub(crate) fn remapping(&self) -> bool {
        self.remapping.load(Ordering::Acquire)
    }

    /// Turn interrupt remapping on, without `CFI`, and read `GSTS` back:
    /// `true` when `CFIS` reads clear, so compatibility-format requests are
    /// blocked from here. When it reads set, remapping is turned off again
    /// and `false` comes back: a unit that remaps but lets compatibility
    /// format through isolates nothing (condition G4).
    ///
    /// # Errors
    ///
    /// The unit never started remapping, or never stopped after `CFIS`
    /// read set.
    pub(crate) fn start_remapping(&self) -> Result<bool, &'static str> {
        self.command(IRE, "it never started remapping interrupts")?;
        if self.registers.read32(GSTS) & CFIS == 0 {
            self.remapping.store(true, Ordering::Release);
            return Ok(true);
        }
        let _held = self.commands.enter()?;
        let standing = self.registers.read32(GSTS) & STANDING & !IRE;
        self.registers.write32(GCMD, standing);
        self.wait(
            || self.registers.read32(GSTS) & IRE == 0,
            "compatibility format still accepted, and it would not stop remapping",
        )?;
        Ok(false)
    }

    /// Write interrupt remapping table entry `index` to deliver as `entry`
    /// says, and make the unit forget any copy of it it cached, before the
    /// message or I/O APIC entry that names it is programmed.
    ///
    /// The entry must not be present: it is never rewritten while it is
    /// (`ferrix_paging::vtd::remap`'s rule). Its high quadword -- the source
    /// ID it checks -- is written first, its low quadword with `P` last, so
    /// the unit never reads a present entry with a stale check. Both are
    /// cleaned to memory on a unit that does not snoop, then the entry is
    /// invalidated index by index and waited for. An index whose
    /// invalidation fails is not used: the caller gets an error and the
    /// vector behind it is never handed out.
    ///
    /// # Errors
    ///
    /// No table, an index past it, an entry already present, a write not
    /// cleaned, or a failed invalidation.
    pub(crate) fn remap(&self, index: u16, entry: Remap) -> Result<(), &'static str> {
        if self.irt == 0 {
            return Err("the unit has no interrupt remapping table");
        }
        if index >= remap::TABLE_ENTRIES {
            return Err("an interrupt entry index past the unit's table");
        }
        let at = self.irt + u64::from(index) * 16;
        if remap::is_present(read_entry(at)) {
            return Err("an interrupt entry in use is never rewritten");
        }
        let [low, high] = remap::entry(entry);
        let mut writes = self.writes();
        write_entry(at + 8, high, &mut writes);
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        write_entry(at, low, &mut writes);
        self.publish(&mut writes);
        settle(&writes);
        require_published(&writes)?;
        self.invalidate_interrupt_entry(InterruptEntryScope::Index(index))
    }

    /// Turn the invalidation queue on: its frame, 256 descriptors of 128
    /// bits, at a tail of 0, with the completion event masked. No
    /// invalidation has been issued yet, so none is in flight.
    fn start_queue(&self) -> Result<(), &'static str> {
        self.registers.write32(IECTL, IECTL_MASKED);
        write64(self.registers, IQA, queue::queue_address(self.queue));
        write64(self.registers, IQT, queue::slot_register(0));
        self.tail.store(0, Ordering::Relaxed);
        self.command(QIE, "it never turned its invalidation queue on")?;
        if queue::slot_of(read64(self.registers, IQH)) != 0 {
            return Err("its invalidation queue did not start at its head");
        }
        Ok(())
    }

    /// Whether a register-based invalidation is pending on the unit: `ICC`
    /// or `IVT` read back set. With the queue on the unit never performs
    /// one -- QEMU's leaves the bit set -- and the kernel issues none, so
    /// stage 10's check R6 requires this false on every unit.
    pub(crate) fn register_invalidation_pending(&self) -> bool {
        read64(self.registers, CCMD) & ICC != 0 || read64(self.registers, self.iotlb) & IVT != 0
    }

    /// The fault the first full recording register holds, cleared so the
    /// unit can record the next, or `None`: the records are read from the
    /// one `FSTS.FRI` names round all `CAP.NFR` + 1 of them
    /// (`ferrix_paging::vtd::unit::fault_records`), as the unit fills them.
    ///
    /// QEMU's unit has one record, as both of the N150's have, and drops a second fault from the same
    /// device while it is full, so a caller that wants a particular fault clears
    /// the record before provoking it.
    ///
    /// **A primary fault overflow comes back too**, as [`Cause::Overflow`], once
    /// the record is empty: `FSTS.PFO`, set when a fault found the record full
    /// and was dropped. It used to be cleared unread with every record taken.
    /// While it is set the unit records nothing, so the record that was full
    /// when the fault was lost is the one this last took, and the overflow is
    /// reported against that record's stream and page: `iommu::provoked` says
    /// what that decides. `FSTS` is read before F, so an overflow is reported
    /// only after the record that was full has been, and a record the unit
    /// fills between the two reads is taken first, with the overflow left for
    /// the next call, as Linux's `dmar_fault` reads every record before it
    /// clears PFO.
    ///
    /// **F is read before the rest of the record, and alone.** A fault
    /// recording register is 128 bits and this kernel reads it 32 at a time,
    /// so the order matters. The unit fills the record before it announces
    /// it: QEMU's `vtd_record_frcd`
    /// writes the low quad and then the high quad with F still clear -- its
    /// comment says "Must not update F field now, should be done later" --
    /// and a second write then sets F. The source id lives in the *low* half
    /// of the high quad and F in the high half, so a read of the whole quad
    /// low-half-first can take the source id from an empty record, be
    /// overtaken by the unit recording a fault, and then read the F bit the
    /// unit just set. The record then reads as a real fault belonging to
    /// stream 0, which is what FX-1001 was: the probe's own page, at stream
    /// 0x0 instead of 0x10, once in a few boots under KVM on a loaded host --
    /// where a vmexit between two halves of a read is likeliest -- and never
    /// under TCG. Reading F by itself first, and the rest only once it is set,
    /// is correct by construction against that write order.
    ///
    /// **A unit marked failed** is reported once, first, as
    /// [`Cause::Queue`]: its queue stopped, and the end-of-boot audit must
    /// count it.
    pub(crate) fn take_fault(&self) -> Option<Fault> {
        let mut taken = self.taken.lock();
        if self.failed.load(Ordering::Acquire)
            && !self.failure_reported.swap(true, Ordering::AcqRel)
        {
            return Some(Fault {
                stream: u32::MAX,
                page: 0,
                write: false,
                cause: Cause::Queue,
            });
        }
        let status = self.registers.read32(FSTS);
        // Every record, from the one `FSTS.FRI` names round: at most 256
        // reads, as `CAP.NFR` is eight bits.
        for record in policy::fault_records(status, self.records) {
            let at = self.record(record);
            let flags = self.registers.read32(at + 12);
            if flags & FRCD_F == 0 {
                continue;
            }
            // F is set, so every other field was written before it and is whole.
            let stream = self.registers.read32(at + 8) & 0xFFFF;
            let low = read64(self.registers, at);
            self.registers.write32(at + 12, FRCD_F);
            let reason = remap::fault_reason(flags);
            // An interrupt request's fault names an entry index, not a page:
            // recorded apart (`L.iommu.54`), and never a page a DMA check
            // provoked, so an overflow behind it counts stray.
            if remap::is_interrupt_reason(reason) {
                *taken = Some((stream, u64::MAX));
                return Some(Fault {
                    stream,
                    page: 0,
                    write: false,
                    cause: Cause::Interrupt {
                        reason,
                        index: remap::fault_index(low),
                    },
                });
            }
            let page = low & !0xFFF;
            *taken = Some((stream, page));
            return Some(Fault {
                stream,
                page,
                write: flags & FRCD_READ == 0,
                cause: Cause::Access,
            });
        }
        if status & PFO == 0 {
            return None;
        }
        self.registers.write32(FSTS, PFO);
        // No record taken since translation went on: nothing a check
        // registered, since a source ID is sixteen bits.
        let (stream, page) = taken.unwrap_or((u32::MAX, 0));
        Some(Fault {
            stream,
            page,
            write: false,
            cause: Cause::Overflow,
        })
    }

    /// Give `function` a domain of its own on this unit: an empty second-level
    /// tree its context entry points at.
    ///
    /// # Errors
    ///
    /// Why it could not: no frames, no identifier left, the function already
    /// attached, or a unit that never finished invalidating.
    pub(crate) fn attach(&self, function: Address) -> Result<Attached, &'static str> {
        let mut writes = self.writes();
        let root = table(&mut writes).ok_or("no frame for a domain's tables")?;
        let installed = self.install(function, root, &mut writes);
        // A failed install may still have linked a new context table.
        self.publish(&mut writes);
        settle(&writes);
        let attached = match installed {
            Ok(attached) => attached,
            Err(why) => {
                mm::deallocate_frames(root / PAGE_SIZE, 0);
                return Err(why);
            }
        };
        // If this fails the entry stays, and so do the tables it points at.
        // Domain 0: an entry not present is cached, in caching mode, under
        // no domain of its own.
        self.invalidate_context(
            ContextScope::Device {
                source: function.requester_id(),
                domain: 0,
            },
            Some(&writes),
        )?;
        Ok(attached)
    }

    /// Write `function`'s context entry to point at `root`, noting each
    /// write in `writes`.
    fn install(
        &self,
        function: Address,
        root: u64,
        writes: &mut Unpublished,
    ) -> Result<Attached, &'static str> {
        let mut tables = self.tables.lock();
        // Room for both records before anything is written, so a failure
        // leaves the unit as it was.
        let held = crate::fallible::reserve().map_err(|_| "no memory to record a domain")?;
        let bus = function.bus();
        let context = match tables.contexts.get(&bus) {
            Some(&context) => context,
            None => {
                let context = table(writes).ok_or("no frame for a context table")?;
                let _ = crate::fallible::insert_held(&held, &mut tables.contexts, bus, context);
                write_entry(self.root + u64::from(bus) * 16, context | PRESENT, writes);
                context
            }
        };
        let entry = context + devfn(function) * 16;
        if read_entry(entry) & PRESENT != 0 {
            return Err("the function already has a domain");
        }
        let identifier = (1..self.identifiers)
            .filter_map(|candidate| u16::try_from(candidate).ok())
            .find(|candidate| !tables.identifiers.contains(candidate))
            .ok_or("the unit has no domain identifier left")?;
        let _ = crate::fallible::insert_into_set_held(&held, &mut tables.identifiers, identifier);
        // The high half first, so the entry is never present with a stale
        // domain identifier or address width.
        write_entry(
            entry + 8,
            self.walk.aw() | u64::from(identifier) << 8,
            writes,
        );
        write_entry(entry, root | PRESENT, writes);
        Ok(Attached {
            function,
            identifier,
            root,
        })
    }

    /// Take `attached`'s context entry away, make the unit forget it, and give
    /// back its root table. Every page in it must have been unmapped, which
    /// also gave back every table below the root.
    ///
    /// # Errors
    ///
    /// Why not; the root table is then kept, since the unit may still walk it.
    pub(crate) fn detach(&self, attached: Attached) -> Result<(), &'static str> {
        let context = self
            .tables
            .lock()
            .contexts
            .get(&attached.function.bus())
            .copied()
            .ok_or("the function's bus has no context table")?;
        let entry = context + devfn(attached.function) * 16;
        let mut writes = self.writes();
        write_entry(entry, 0, &mut writes);
        write_entry(entry + 8, 0, &mut writes);
        // In memory before the invalidation, or a unit that does not snoop
        // could still read the entry as present and reach the frames below.
        self.publish(&mut writes);
        settle(&writes);
        // Under the domain the entry was present with, as the unit cached it.
        self.invalidate_context(
            ContextScope::Device {
                source: attached.function.requester_id(),
                domain: attached.identifier,
            },
            Some(&writes),
        )?;
        // The same record, checked by the invalidation just made.
        self.invalidate_iotlb(IotlbScope::Domain(attached.identifier), None, Wait::Status)?;
        let _ = self.tables.lock().identifiers.remove(&attached.identifier);
        mm::deallocate_frames(attached.root / PAGE_SIZE, 0);
        Ok(())
    }

    /// Map the page at `phys` at I/O address `iova` in `attached`'s tables.
    ///
    /// # Errors
    ///
    /// What the tables refused.
    pub(crate) fn map(
        &self,
        attached: &Attached,
        iova: u64,
        phys: u64,
        flags: MapFlags,
    ) -> Result<(), MapError> {
        let mut writes = self.writes();
        let mapped = match self.walk {
            Walk::Three => {
                mm::map_io::<VtdSecondLevel>(attached.root, iova, phys, flags, &mut writes)
            }
            Walk::Four => mm::map_io::<VtdFourLevel>(attached.root, iova, phys, flags, &mut writes),
        };
        // Published here: outside caching mode no invalidation follows a map,
        // and the device may use the address once this returns.
        self.publish(&mut writes);
        settle(&writes);
        // Refused rather than published: the caller unwinds the pin.
        require_published(&writes).map_err(|_| MapError::NotCleaned)?;
        mapped
    }

    /// Take the page at `iova` out of `attached`'s tables. The unit may still
    /// reach it until [`Unit::flush`] returns.
    ///
    /// # Errors
    ///
    /// What the tables refused.
    pub(crate) fn unmap(
        &self,
        attached: &Attached,
        iova: u64,
        tables: &mut mm::UnlinkedTables,
    ) -> Result<(), MapError> {
        let mut writes = self.writes();
        let unmapped = match self.walk {
            Walk::Three => mm::unmap_io::<VtdSecondLevel>(attached.root, iova, tables, &mut writes),
            Walk::Four => mm::unmap_io::<VtdFourLevel>(attached.root, iova, tables, &mut writes),
        };
        // In memory before the caller's [`Unit::flush`] invalidates the IOTLB.
        self.publish(&mut writes);
        settle(&writes);
        // Refused rather than flushed: the caller keeps the frames, as it
        // does for an invalidation that never finished.
        require_published(&writes).map_err(|_| MapError::NotCleaned)?;
        unmapped
    }

    /// Where `attached`'s tables send an access to `iova`, walked as the unit
    /// walks them.
    pub(crate) fn resolve(&self, attached: &Attached, iova: u64) -> Option<u64> {
        match self.walk {
            Walk::Three => mm::translate_io::<VtdSecondLevel>(attached.root, iova),
            Walk::Four => mm::translate_io::<VtdFourLevel>(attached.root, iova),
        }
    }

    /// Make the unit forget what it cached of `attached`'s tables: after an
    /// unmap always, and after a map only in caching mode, where not-present
    /// entries are cached too.
    ///
    /// `wait` is [`Wait::Status`] everywhere but in check R7, which passes
    /// [`Wait::Unwritten`] to make the invalidation fail.
    ///
    /// # Errors
    ///
    /// A unit that never finished.
    pub(crate) fn flush(
        &self,
        attached: &Attached,
        after_map: bool,
        wait: Wait,
    ) -> Result<(), &'static str> {
        if after_map && !self.caching {
            return Ok(());
        }
        // `map` and `unmap` checked their own records before they returned.
        self.invalidate_iotlb(IotlbScope::Domain(attached.identifier), None, wait)
    }

    /// A record for this unit's table writes: one that notes and cleans
    /// them if the unit's walk does not snoop, and one that does nothing if
    /// it does. Every change to the tables -- N0g's interrupt remapping table
    /// and invalidation queue included -- writes through one, with
    /// [`write_entry`] and [`table`], and [`Unit::publish`]es it before the
    /// unit is told.
    pub(crate) fn writes(&self) -> Unpublished {
        Unpublished::new(self.coherent)
    }

    /// Clean every write `writes` noted to memory, and wait until it is
    /// there.
    pub(crate) fn publish(&self, writes: &mut Unpublished) {
        writes.publish(&mut mm::WalkerClean);
    }

    /// Set `bit` in GCMD, keeping every standing enable, and wait for GSTS to
    /// report it.
    fn command(&self, bit: u32, why: &'static str) -> Result<(), &'static str> {
        let _held = self.commands.enter()?;
        let standing = self.registers.read32(GSTS) & STANDING;
        self.registers.write32(GCMD, standing | bit);
        self.wait(|| self.registers.read32(GSTS) & bit != 0, why)
    }

    /// Invalidate the context cache for `scope` through the queue, and wait
    /// until it has. Refused if `writes`, the record of the change this
    /// publishes, holds a write not cleaned to memory; `None` for an
    /// invalidation that publishes no change's writes.
    fn invalidate_context(
        &self,
        scope: ContextScope,
        writes: Option<&Unpublished>,
    ) -> Result<(), &'static str> {
        if let Some(writes) = writes {
            require_published(writes)?;
        }
        self.submit(queue::context(scope), Wait::Status)?;
        let _ = QUEUED_CONTEXT.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Invalidate the interrupt entry cache for `scope` through the queue,
    /// and wait until it has.
    fn invalidate_interrupt_entry(&self, scope: InterruptEntryScope) -> Result<(), &'static str> {
        self.submit(queue::interrupt_entry(scope), Wait::Status)?;
        let _ = QUEUED_INTERRUPT_ENTRY.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Invalidate the IOTLB for `scope` through the queue, and wait until it
    /// has, as `wait` says. Refused as [`Unit::invalidate_context`] is.
    fn invalidate_iotlb(
        &self,
        scope: IotlbScope,
        writes: Option<&Unpublished>,
        wait: Wait,
    ) -> Result<(), &'static str> {
        if let Some(writes) = writes {
            require_published(writes)?;
        }
        self.submit(queue::iotlb(scope, self.drain), wait)?;
        let _ = QUEUED_IOTLB.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Put `descriptor` and a fenced wait behind it in the queue, move the
    /// tail past both, and wait for the wait to complete: its status word to
    /// read this submission's sequence number, within [`PATIENCE_NANOS`].
    ///
    /// Both descriptors are cleaned to memory, on a unit that does not
    /// snoop, before the tail write that publishes them. Held inside
    /// `commands` throughout, so two submissions never interleave; the wait
    /// is made with interrupts on wherever the caller may block (`gate`).
    ///
    /// With [`Wait::Unwritten`] the wait writes no status, so this fails
    /// after [`UNWRITTEN_PATIENCE_NANOS`]: check R7's way of making an
    /// invalidation fail.
    ///
    /// # Errors
    ///
    /// A failed invalidation: the unit already failed, no room in the queue,
    /// a status not written in time, or a queue error seen while waiting --
    /// which also marks the unit failed.
    fn submit(&self, descriptor: Descriptor, wait: Wait) -> Result<(), &'static str> {
        let result = self.submit_and_wait(descriptor, wait);
        if result.is_err() {
            let _ = INVALIDATIONS_FAILED.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// [`Unit::submit`]'s work.
    fn submit_and_wait(&self, descriptor: Descriptor, wait: Wait) -> Result<(), &'static str> {
        if self.failed.load(Ordering::Acquire) {
            return Err(FAILED_WHY);
        }
        let _held = self.commands.enter()?;
        let tail = self.tail.load(Ordering::Relaxed);
        let head = queue::slot_of(read64(self.registers, IQH));
        if queue::free_slots(head, tail) < 2 {
            return Err("its invalidation queue had no room");
        }
        let sequence = match self.sequence.load(Ordering::Relaxed).wrapping_add(1) {
            0 => 1,
            next => next,
        };
        self.sequence.store(sequence, Ordering::Relaxed);
        let completion = match wait {
            Wait::Status => Completion::Status {
                address: self.status,
                data: sequence,
            },
            Wait::Unwritten => Completion::Unwritten,
        };
        let slot = self.write_descriptors(tail, [descriptor, queue::wait(completion)])?;
        self.tail.store(slot, Ordering::Relaxed);
        write64(self.registers, IQT, queue::slot_register(slot));
        self.await_completion(sequence, wait)
    }

    /// Write `descriptors` into the queue from slot `tail` on, and clean them
    /// to memory, so that the unit fetches them whole once the tail moves:
    /// the slot after the last.
    ///
    /// # Errors
    ///
    /// [`UNCLEANED_WHY`], for a descriptor not cleaned.
    fn write_descriptors(
        &self,
        tail: u32,
        descriptors: [Descriptor; 2],
    ) -> Result<u32, &'static str> {
        let mut writes = self.writes();
        let mut slot = tail;
        for written in descriptors {
            let at = self.queue + u64::from(slot) * queue::DESCRIPTOR_BYTES;
            write_entry(at, written[0], &mut writes);
            write_entry(at + 8, written[1], &mut writes);
            slot = (slot + 1) % queue::QUEUE_LENGTH;
        }
        // In memory before the tail write the unit fetches them after.
        self.publish(&mut writes);
        settle(&writes);
        if writes.is_published() {
            Ok(slot)
        } else {
            Err(UNCLEANED_WHY)
        }
    }

    /// Wait for the wait descriptor whose status is `sequence` to complete,
    /// within the patience `wait` has, and see whether the unit stopped or
    /// failed it.
    ///
    /// # Errors
    ///
    /// A queue error -- which for `IQE` or `ITE` also marks the unit failed --
    /// or a status not written in time.
    fn await_completion(&self, sequence: u32, wait: Wait) -> Result<(), &'static str> {
        let patience = match wait {
            Wait::Status => PATIENCE_NANOS,
            Wait::Unwritten => UNWRITTEN_PATIENCE_NANOS,
        };
        let deadline = timer::now_nanos().saturating_add(patience);
        let _ = gate::poll(
            || self.status_word() == sequence || self.fault_status() & FSTS_QUEUE_ERRORS != 0,
            deadline,
        );
        let status = self.fault_status();
        match queue::outcome(status, self.status_word() == sequence) {
            Outcome::Completed => Ok(()),
            Outcome::TimedOut => Err("it never finished an invalidation"),
            Outcome::QueueStopped => {
                self.fail(status);
                Err(FAILED_WHY)
            }
            // Cleared at once, write-one-to-clear: left set, it would fail
            // every later wait on a unit never marked failed.
            Outcome::CompletionError => {
                self.clear_errors(FSTS_ICE);
                let _ = COMPLETION_ERRORS.fetch_add(1, Ordering::Relaxed);
                Err("it reported an invalidation completion error")
            }
        }
    }

    /// The fault status register, with any queue error a boot check
    /// planted.
    fn fault_status(&self) -> u32 {
        self.registers.read32(FSTS) | self.planted.load(Ordering::Relaxed)
    }

    /// Clear the write-one-to-clear queue errors in `bits`, in the register
    /// and among those a check planted.
    fn clear_errors(&self, bits: u32) {
        self.registers.write32(FSTS, bits & FSTS_QUEUE_ERRORS);
        let _ = self.planted.fetch_and(!bits, Ordering::Relaxed);
    }

    /// The word the queue's wait descriptors write their status to.
    fn status_word(&self) -> u32 {
        // SAFETY: (DMA) `status` is a frame this unit took from the frame
        // allocator for itself in `open` and never gives back; the direct map
        // covers every frame of RAM, and its first word is four-byte
        // aligned. The unit writes it, which is why the read is volatile.
        unsafe { core::ptr::read_volatile(mm::direct_map(self.status) as *const u32) }
    }

    /// Mark the unit failed after its queue stopped, with `status` the fault
    /// status register that said so, and say so once.
    fn fail(&self, status: u32) {
        if self.failed.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = UNITS_FAILED.fetch_add(1, Ordering::Relaxed);
        crate::println!(
            "  iommu    VT-d unit {:#x}: its invalidation queue stopped ({}): it takes no \
             invalidation again, and nothing it may reach is released until reboot",
            self.phys,
            if status & FSTS_IQE != 0 {
                "IQE, a descriptor it would not take"
            } else {
                "ITE, an invalidation that timed out"
            },
        );
    }

    /// Wait for `ready`, up to [`PATIENCE_NANOS`], with interrupts on
    /// wherever the caller may block: see `gate`.
    fn wait(&self, ready: impl Fn() -> bool, why: &'static str) -> Result<(), &'static str> {
        wait_for(ready, why)
    }
}

/// Say why the unit at `phys`, whose registers read `ver`, `cap` and
/// `ecap`, is left alone, and answer the reason.
fn say_refused(phys: u64, ver: u32, cap: u64, ecap: u64, why: &'static str) -> &'static str {
    crate::println!(
        "  iommu    vt-d unit {phys:#x}: VER {}.{}, CAP {cap:#x}, ECAP {ecap:#x}: left alone: \
         {why}",
        policy::major(ver),
        policy::minor(ver)
    );
    why
}

/// Bring `unit`'s registers into the state firmware that used the
/// invalidation queue would leave them in -- the queue on, at the unit's own
/// frame, its last descriptor a wait the unit has completed -- and require
/// [`stop_firmware`] to turn it off and read it back off: the path `open`
/// takes on such a unit, which no firmware QEMU boots takes. For
/// `iommu/check.rs`, before `enable` turns the queue on for good.
///
/// The wait matters: QEMU will not turn a queue off whose last descriptor
/// was anything else (`vtd_queued_inv_disable_check`), and firmware that
/// waits for its invalidations, as any must, leaves one last.
///
/// # Errors
///
/// The queue would not go on as firmware's would, or `stop_firmware` did
/// not turn it off.
pub(super) fn leave_queue_on_and_stop(unit: &Unit) -> Result<(), &'static str> {
    let registers = unit.registers;
    registers.write32(IECTL, IECTL_MASKED);
    write64(registers, IQA, queue::queue_address(unit.queue));
    write64(registers, IQT, queue::slot_register(0));
    registers.write32(GCMD, (registers.read32(GSTS) & STANDING) | QIE);
    wait_for(
        || registers.read32(GSTS) & QIE != 0,
        "a queue turned on as firmware would never came on",
    )?;
    /// What firmware's last wait writes, which no sequence number reaches
    /// before the boot's checks are over.
    const FIRMWARE_WAIT: u32 = 0xF1A7_0000;
    let mut writes = unit.writes();
    let last = queue::wait(Completion::Status {
        address: unit.status,
        data: FIRMWARE_WAIT,
    });
    write_entry(unit.queue, last[0], &mut writes);
    write_entry(unit.queue + 8, last[1], &mut writes);
    unit.publish(&mut writes);
    settle(&writes);
    write64(registers, IQT, queue::slot_register(1));
    wait_for(
        || unit.status_word() == FIRMWARE_WAIT,
        "a wait in a queue turned on as firmware would never completed",
    )?;
    stop_firmware(registers)?;
    if registers.read32(GSTS) & QIE != 0 {
        return Err("a queue firmware left on was still on after it was stopped");
    }
    Ok(())
}

/// The frames a unit is given, each zeroed and, on a unit that does not
/// snoop, cleaned whole before anything links it (G8): its root table, its
/// invalidation queue, the queue's status word and -- when `remaps` -- its
/// interrupt remapping table, else 0 for it. None, with every frame taken
/// given back, when one cannot be had.
fn unit_frames(writes: &mut Unpublished, remaps: bool) -> Option<[u64; 4]> {
    let wanted = if remaps { 4 } else { 3 };
    let mut frames = [0; 4];
    for at in 0..wanted {
        match table(writes) {
            Some(frame) => {
                if let Some(slot) = frames.get_mut(at) {
                    *slot = frame;
                }
            }
            None => {
                for &taken in frames.iter().take(at) {
                    mm::deallocate_frames(taken / PAGE_SIZE, 0);
                }
                return None;
            }
        }
    }
    Some(frames)
}

/// Turn off what firmware left on that the kernel sets up itself: interrupt
/// remapping, then the invalidation queue once it is idle. Each must read
/// back off within [`PATIENCE_NANOS`]. Then any queue error firmware left
/// in `FSTS` is cleared ([`clear_stale_errors`]). A table pointer firmware
/// left (`IRTPS`) needs nothing here: the kernel's own replaces it before
/// anything uses it.
///
/// # Errors
///
/// What firmware left on and would not stop, or a queue error that would
/// not clear.
fn stop_firmware(registers: Mmio) -> Result<(), &'static str> {
    let status = registers.read32(GSTS);
    if status & IRE != 0 {
        registers.write32(GCMD, status & STANDING & !IRE);
        wait_for(
            || registers.read32(GSTS) & IRE == 0,
            "firmware left it remapping interrupts and it would not stop",
        )?;
    }
    if registers.read32(GSTS) & QIE != 0 {
        wait_for(
            || queue::slot_of(read64(registers, IQH)) == queue::slot_of(read64(registers, IQT)),
            "firmware left its invalidation queue busy and it would not drain",
        )?;
        registers.write32(GCMD, registers.read32(GSTS) & STANDING & !QIE);
        wait_for(
            || registers.read32(GSTS) & QIE == 0,
            "firmware left its invalidation queue on and it would not stop",
        )?;
    }
    clear_stale_errors(registers)
}

/// Clear any queue error -- `IQE`, `ICE`, `ITE` -- firmware left in
/// `FSTS`, each write-one-to-clear, and read the register back clear: a
/// stale bit would fail the kernel's first invalidation.
///
/// # Errors
///
/// A queue error that would not clear.
fn clear_stale_errors(registers: Mmio) -> Result<(), &'static str> {
    let stale = registers.read32(FSTS) & FSTS_QUEUE_ERRORS;
    if stale == 0 {
        return Ok(());
    }
    registers.write32(FSTS, stale);
    if registers.read32(FSTS) & FSTS_QUEUE_ERRORS != 0 {
        return Err("firmware left an invalidation queue error that would not clear");
    }
    Ok(())
}

/// Plant an invalidation completion error (`ICE`) on `unit`, as if the
/// unit had raised one, and require an invalidation to fail on it, the
/// error to be cleared and counted, and the next invalidation to succeed
/// with the unit not marked failed (the consultant's condition 1 on N0g's
/// slice 1). For `iommu/check.rs`; QEMU's unit never raises `ICE`.
///
/// # Errors
///
/// What did not hold.
pub(super) fn check_planted_completion_error(unit: &Unit) -> Result<(), &'static str> {
    let cleared = COMPLETION_ERRORS.load(Ordering::Relaxed);
    let _ = unit.planted.fetch_or(FSTS_ICE, Ordering::Relaxed);
    if unit
        .invalidate_iotlb(IotlbScope::Global, None, Wait::Status)
        .is_ok()
    {
        unit.clear_errors(FSTS_ICE);
        return Err("an invalidation succeeded with a completion error set");
    }
    if COMPLETION_ERRORS.load(Ordering::Relaxed) != cleared + 1 {
        unit.clear_errors(FSTS_ICE);
        return Err("a completion error was not counted");
    }
    if unit
        .invalidate_iotlb(IotlbScope::Global, None, Wait::Status)
        .is_err()
    {
        return Err("an invalidation after a cleared completion error failed");
    }
    if unit.failed.load(Ordering::Acquire) {
        return Err("a completion error marked the unit failed");
    }
    Ok(())
}

/// Wait for `ready`, up to [`PATIENCE_NANOS`], with interrupts on wherever
/// the caller may block: see `gate`.
fn wait_for(ready: impl Fn() -> bool, why: &'static str) -> Result<(), &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if gate::poll(ready, deadline) {
        Ok(())
    } else {
        Err(why)
    }
}

/// A function's index in its bus's context table.
fn devfn(function: Address) -> u64 {
    u64::from(function.device()) << 3 | u64::from(function.function())
}

/// A zeroed frame for a table the unit walks, by physical address: a root,
/// context or second-level root table, and N0g's interrupt remapping table
/// and invalidation queue.
///
/// On a unit that does not snoop, the frame is cleaned to memory whole
/// before it is returned, so before anything can link it: stale memory
/// under a fresh table reads as present entries.
pub(crate) fn table(writes: &mut Unpublished) -> Option<u64> {
    let frame = mm::allocate_frames(0)?;
    mm::zero_frame(frame);
    writes.fresh_table(PhysAddr(frame * PAGE_SIZE), &mut mm::WalkerClean);
    Some(frame * PAGE_SIZE)
}

/// Count what `writes` noted, once its change is done.
fn settle(writes: &Unpublished) {
    let (entries, tables) = writes.counts();
    let _ = ENTRIES_CLEANED.fetch_add(entries, Ordering::Relaxed);
    let _ = TABLES_CLEANED.fetch_add(tables, Ordering::Relaxed);
}

/// The check at a change's own publish point: every write `writes` noted
/// has been cleaned to memory. Counted either way, for stage 10's check.
///
/// # Errors
///
/// [`UNCLEANED_WHY`], for a write not cleaned, which is printed too.
fn require_published(writes: &Unpublished) -> Result<(), &'static str> {
    let _ = CHANGES_CHECKED.fetch_add(1, Ordering::Relaxed);
    if writes.is_published() {
        Ok(())
    } else {
        let _ = UNCLEANED.fetch_add(1, Ordering::Relaxed);
        // Named where it happens: what the refusal makes fail next -- a pin,
        // an unpin, a domain -- does not say why.
        crate::println!("  iommu    a VT-d change was refused: {UNCLEANED_WHY}");
        Err(UNCLEANED_WHY)
    }
}

/// Read the table entry at physical address `at`.
fn read_entry(at: u64) -> u64 {
    // SAFETY: (DMA) `at` is an entry inside a root or context table this unit took
    // from the frame allocator for itself and never gave back, at an offset
    // that is a multiple of eight within the page; the direct map covers every
    // frame of RAM. The unit reads the entry too, which is why the access is
    // volatile.
    unsafe { core::ptr::read_volatile(mm::direct_map(at) as *const u64) }
}

/// Write the table entry at physical address `at`, and note it in `writes`
/// to be cleaned before it is published.
pub(crate) fn write_entry(at: u64, value: u64, writes: &mut Unpublished) {
    // SAFETY: (DMA) as `read_entry`: a whole, aligned eight-byte entry in a table
    // only this unit's code writes.
    unsafe { core::ptr::write_volatile(mm::direct_map(at) as *mut u64, value) };
    writes.wrote(PhysAddr(at), 8, &mut mm::WalkerClean);
}

/// Read a 64-bit register as two 32-bit halves, low first.
fn read64(registers: Mmio, at: u64) -> u64 {
    u64::from(registers.read32(at)) | u64::from(registers.read32(at + 4)) << 32
}

/// Write a 64-bit register as two 32-bit halves, low first: a command in the
/// high half takes effect when that half is written, with the low half already
/// in place.
fn write64(registers: Mmio, at: u64, value: u64) {
    registers.write32(at, value as u32);
    registers.write32(at + 4, (value >> 32) as u32);
}

impl Unit {
    /// Give `function` a domain of its own holding exactly `regions`'
    /// pages, each readable and writable at its own address, and nothing
    /// else: an RMRR's identity domain (`vtd-design.md` §3.9(b)), made
    /// after [`Unit::enable`] and before [`Unit::translate`], and never
    /// detached (`L.iommu.73`, `L.iommu.78`). The regions were honoured by
    /// `ferrix_paging::vtd::rmrr::identity` and share no page.
    ///
    /// # Errors
    ///
    /// Why not: [`Unit::attach`]'s reasons, a page the tables would not
    /// take, or a unit that never finished. What was made is taken down
    /// again first; if the unit refuses even that, the tables are kept and
    /// the answer says so -- the unit is then never told to translate, so
    /// nothing walks them.
    pub(crate) fn attach_identity(
        &self,
        function: Address,
        regions: &[ferrix_paging::vtd::rmrr::Identity],
    ) -> Result<Attached, &'static str> {
        // Before translation goes on, but for stage 10's check, which makes
        // one on QEMU's translating unit (`domain_check::check_rmrr_identity`).
        if self.translating() && !crate::boot_checks::run() {
            return Err("an RMRR identity domain is made before its unit translates");
        }
        let attached = self.attach(function)?;
        let mut mapped: u64 = 0;
        let mut refused = None;
        'regions: for region in regions {
            for page in region.page_addresses() {
                match self.map(&attached, page, page, MapFlags::DMA) {
                    Ok(()) => mapped += 1,
                    // A map refused for a write not cleaned wrote its page.
                    Err(MapError::NotCleaned) => {
                        mapped += 1;
                        refused = Some("an RMRR page was not cleaned to memory");
                        break 'regions;
                    }
                    Err(_) => {
                        refused = Some("an RMRR page could not be mapped");
                        break 'regions;
                    }
                }
            }
        }
        let flushed = match refused {
            None => self.flush(&attached, true, Wait::Status),
            Some(why) => Err(why),
        };
        let Err(why) = flushed else {
            return Ok(attached);
        };
        let mut tables = mm::UnlinkedTables::of(mm::TableOwner::Device);
        let taken = regions
            .iter()
            .flat_map(|region| region.page_addresses())
            .take(usize::try_from(mapped).unwrap_or(usize::MAX))
            .all(|page| self.unmap(&attached, page, &mut tables).is_ok());
        if taken && self.flush(&attached, false, Wait::Status).is_ok() {
            tables.release();
            if self.detach(attached).is_ok() {
                return Err(why);
            }
        }
        Err("an RMRR identity domain could not be made, nor taken down: its tables are kept")
    }
}
