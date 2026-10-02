//! Intel VT-d in legacy mode: a remapping unit's root and context tables, and
//! the second-level tables of the domains built on it.
//!
//! Written against the specification and checked against QEMU's
//! `hw/i386/intel_iommu.c`, which the boot test runs. Legacy mode needs, and
//! this does:
//!
//! * a **root table**, one entry per bus, pointing at that bus's **context
//!   table**, one entry per device and function, which names a domain and the
//!   root of its second-level tables — `src/lib/kernel/paging`'s [`VtdSecondLevel`],
//!   three levels over 39 bits;
//! * **register-based invalidation** of the context cache and the IOTLB, after
//!   anything the unit may have cached changes;
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
//! before anything links it ([`table`]). Each invalidation, and the end of
//! each change, checks the record is empty, and counts what it finds for
//! stage 10's check (finding F-58). QEMU's unit reports `C` clear, though it
//! walks coherently, so every boot test runs this path. A unit with `C` set
//! is written as before, with nothing noted or cleaned.
//!
//! Queued invalidation, interrupt remapping and fault events are not used.
//! QEMU honours the register interface while queued invalidation is off, and
//! the kernel's MSI-X messages are compatibility format, which QEMU passes
//! through untouched until interrupt remapping is enabled.

use alloc::collections::{BTreeMap, BTreeSet};
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::coherence::Unpublished;
use ferrix_paging::vtd::VtdSecondLevel;
use ferrix_paging::{MapError, MapFlags, PhysAddr};
use ferrix_pci::Address;
use ferrix_sync::IrqSpinLock;

use super::gate::{self, Gate};
use super::{Cause, Fault};
use crate::mmio::Mmio;
use crate::{arch, mm, timer, vmap};

/// Bytes of registers mapped: every register legacy mode uses is in the first
/// page.
const WINDOW: u64 = 0x1000;

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
/// Context command register.
const CCMD: u64 = 0x28;
/// Fault status register.
const FSTS: u64 = 0x34;

/// GCMD: turn translation on; GSTS: it is on.
const TE: u32 = 1 << 31;
/// GCMD: take the root table pointer; GSTS: it is taken.
const SRTP: u32 = 1 << 30;
/// GSTS bits reporting a standing enable, which every GCMD write repeats so as
/// not to turn it off: translation, queued invalidation, interrupt remapping
/// and compatibility-format interrupts.
const STANDING: u32 = 1 << 31 | 1 << 26 | 1 << 25 | 1 << 23;

/// ECAP: page-walk coherency, the unit's walk snoops the processors' caches.
/// Clear, every table write is cleaned to memory before it is published.
const ECAP_C: u64 = 1 << 0;

/// CAP: the unit needs its write buffer flushed after every change, which this
/// driver does not do.
const CAP_RWBF: u64 = 1 << 4;
/// CAP: caching mode, in which even an entry that was not present is cached
/// and must be invalidated once it is.
const CAP_CM: u64 = 1 << 7;
/// CAP: the `SAGAW` bit for three-level, 39-bit tables.
const CAP_SAGAW_39: u64 = 1 << 9;

/// CCMD: invalidate the context cache; reads back set until done.
const ICC: u64 = 1 << 63;
/// CCMD: one source ID, named in bits 31:16.
const CCMD_DEVICE: u64 = 3 << 61;
/// CCMD: every entry.
const CCMD_GLOBAL: u64 = 1 << 61;

/// IOTLB register: invalidate; reads back set until done.
const IVT: u64 = 1 << 63;
/// IOTLB register: every entry.
const IOTLB_GLOBAL: u64 = 1 << 60;
/// IOTLB register: one domain, named in bits 47:32.
const IOTLB_DOMAIN: u64 = 2 << 60;

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
/// Context entry, high half: the address width field for three levels.
const AW_39: u64 = 1;

/// How long a command may take before the unit is given up on.
const PATIENCE_NANOS: u64 = 100_000_000;

/// Why a publish was refused: a table write not cleaned to memory.
const UNCLEANED_WHY: &str = "a table write was not cleaned to memory before it was published";

/// Units opened whose walk does not snoop.
static UNITS_CLEANING: AtomicU64 = AtomicU64::new(0);
/// Entry writes cleaned to memory on those units.
static ENTRIES_CLEANED: AtomicU64 = AtomicU64::new(0);
/// Fresh tables cleaned whole on those units.
static TABLES_CLEANED: AtomicU64 = AtomicU64::new(0);
/// Publish points that checked their record: invalidations and the end of
/// each change.
static PUBLISHES_CHECKED: AtomicU64 = AtomicU64::new(0);
/// Publish points that found a write not cleaned.
static UNCLEANED: AtomicU64 = AtomicU64::new(0);

/// What the units' cleaning did, for stage 10's check.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cleaning {
    /// Units whose walk does not snoop.
    pub(crate) units: u64,
    /// Entry writes cleaned to memory.
    pub(crate) entries: u64,
    /// Fresh tables cleaned whole.
    pub(crate) tables: u64,
    /// Publish points checked.
    pub(crate) checked: u64,
    /// Publish points that found a write not cleaned.
    pub(crate) uncleaned: u64,
}

/// What every unit's cleaning has done since boot.
pub(crate) fn cleaning() -> Cleaning {
    Cleaning {
        units: UNITS_CLEANING.load(Ordering::Relaxed),
        entries: ENTRIES_CLEANED.load(Ordering::Relaxed),
        tables: TABLES_CLEANED.load(Ordering::Relaxed),
        checked: PUBLISHES_CHECKED.load(Ordering::Relaxed),
        uncleaned: UNCLEANED.load(Ordering::Relaxed),
    }
}

/// One remapping unit.
#[derive(Debug)]
pub(crate) struct Unit {
    /// Its registers.
    registers: Mmio,
    /// Physical address of the root table.
    root: u64,
    /// Offset of the first fault recording register.
    faults: u64,
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
    /// do what this driver asks.
    ///
    /// # Errors
    ///
    /// Why the unit is left alone.
    pub(crate) fn open(phys: u64) -> Result<Unit, &'static str> {
        let base =
            vmap::map_device(phys, WINDOW).map_err(|_| "its registers could not be mapped")?;
        let registers = Mmio::at(base);
        let refuse = |why: &'static str| -> Result<Unit, &'static str> {
            let _ = vmap::unmap_device(base);
            Err(why)
        };
        if (registers.read32(VER) >> 4) & 0xF != 1 {
            return refuse("it is not a version 1 unit");
        }
        let cap = read64(registers, CAP);
        let ecap = read64(registers, ECAP);
        if cap & CAP_SAGAW_39 == 0 {
            return refuse("it cannot walk three-level tables");
        }
        if cap & CAP_RWBF != 0 {
            return refuse("it needs its write buffer flushed");
        }
        if registers.read32(GSTS) & TE != 0 {
            return refuse("firmware left it translating");
        }
        let coherent = ecap & ECAP_C != 0;
        let mut writes = Unpublished::new(coherent);
        let Some(root) = table(&mut writes) else {
            return refuse("no frame for its root table");
        };
        settle(&writes);
        if !coherent {
            let _ = UNITS_CLEANING.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Unit {
            registers,
            root,
            faults: ((cap >> 24) & 0x3FF) * 16,
            iotlb: ((ecap >> 8) & 0x3FF) * 16 + 8,
            caching: cap & CAP_CM != 0,
            coherent,
            identifiers: 1 << (4 + 2 * (cap & 0b111)),
            tables: IrqSpinLock::new(Tables::default()),
            taken: IrqSpinLock::new(None),
            commands: Gate::new(),
        })
    }

    /// Point the unit at its root table, make it forget what it cached, and
    /// turn translation on. From here a function with no context entry reaches
    /// nothing.
    ///
    /// # Errors
    ///
    /// The command the unit never finished.
    pub(crate) fn enable(&self) -> Result<(), &'static str> {
        write64(self.registers, RTADDR, self.root);
        self.command(SRTP, "it never took its root table")?;
        // The root table was cleaned when `open` made it, and nothing has
        // been written since.
        let writes = self.writes();
        self.invalidate_context(CCMD_GLOBAL, &writes)?;
        self.invalidate_iotlb(IOTLB_GLOBAL, &writes)?;
        // A fault firmware left recorded would otherwise be read as ours.
        if self.registers.read32(self.faults + 12) & FRCD_F != 0 {
            self.registers.write32(self.faults + 12, FRCD_F);
        }
        self.registers.write32(FSTS, PFO);
        *self.taken.lock() = None;
        self.command(TE, "it never started translating")
    }

    /// The fault the unit's first recording register holds, cleared so the unit
    /// can record the next, or `None`.
    ///
    /// QEMU's unit has one record, and drops a second fault from the same
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
    pub(crate) fn take_fault(&self) -> Option<Fault> {
        let mut taken = self.taken.lock();
        let status = self.registers.read32(FSTS);
        let flags = self.registers.read32(self.faults + 12);
        if flags & FRCD_F != 0 {
            // F is set, so every other field was written before it and is whole.
            let stream = self.registers.read32(self.faults + 8) & 0xFFFF;
            let page = read64(self.registers, self.faults) & !0xFFF;
            self.registers.write32(self.faults + 12, FRCD_F);
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
        self.invalidate_context(
            CCMD_DEVICE | u64::from(function.requester_id()) << 16,
            &writes,
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
        write_entry(entry + 8, AW_39 | u64::from(identifier) << 8, writes);
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
        self.invalidate_context(
            CCMD_DEVICE | u64::from(attached.function.requester_id()) << 16,
            &writes,
        )?;
        self.invalidate_iotlb(IOTLB_DOMAIN | u64::from(attached.identifier) << 32, &writes)?;
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
        let mapped = mm::map_io::<VtdSecondLevel>(attached.root, iova, phys, flags, &mut writes);
        // Published here: outside caching mode no invalidation follows a map,
        // and the device may use the address once this returns.
        self.publish(&mut writes);
        settle(&writes);
        let _ = require_published(&writes);
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
        let unmapped = mm::unmap_io::<VtdSecondLevel>(attached.root, iova, tables, &mut writes);
        // In memory before the caller's [`Unit::flush`] invalidates the IOTLB.
        self.publish(&mut writes);
        settle(&writes);
        let _ = require_published(&writes);
        unmapped
    }

    /// Where `attached`'s tables send an access to `iova`, walked as the unit
    /// walks them.
    pub(crate) fn resolve(&self, attached: &Attached, iova: u64) -> Option<u64> {
        mm::translate_io::<VtdSecondLevel>(attached.root, iova)
    }

    /// Make the unit forget what it cached of `attached`'s tables: after an
    /// unmap always, and after a map only in caching mode, where not-present
    /// entries are cached too.
    ///
    /// # Errors
    ///
    /// A unit that never finished.
    pub(crate) fn flush(&self, attached: &Attached, after_map: bool) -> Result<(), &'static str> {
        if after_map && !self.caching {
            return Ok(());
        }
        // `map` and `unmap` cleaned what they wrote before they returned.
        self.invalidate_iotlb(
            IOTLB_DOMAIN | u64::from(attached.identifier) << 32,
            &self.writes(),
        )
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

    /// Invalidate the context cache for `scope`, and wait until it has.
    /// Refused if `writes`, the record of what the change wrote, holds a
    /// write not cleaned to memory.
    fn invalidate_context(&self, scope: u64, writes: &Unpublished) -> Result<(), &'static str> {
        require_published(writes)?;
        let _held = self.commands.enter()?;
        write64(self.registers, CCMD, ICC | scope);
        self.wait(
            || read64(self.registers, CCMD) & ICC == 0,
            "it never finished invalidating its context cache",
        )
    }

    /// Invalidate the IOTLB for `scope`, and wait until it has. Refused as
    /// [`Unit::invalidate_context`] is.
    fn invalidate_iotlb(&self, scope: u64, writes: &Unpublished) -> Result<(), &'static str> {
        require_published(writes)?;
        let _held = self.commands.enter()?;
        write64(self.registers, self.iotlb, IVT | scope);
        self.wait(
            || read64(self.registers, self.iotlb) & IVT == 0,
            "it never finished invalidating its IOTLB",
        )
    }

    /// Wait for `ready`, up to [`PATIENCE_NANOS`], with interrupts on
    /// wherever the caller may block: see `gate`.
    fn wait(&self, ready: impl Fn() -> bool, why: &'static str) -> Result<(), &'static str> {
        let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
        if gate::poll(ready, deadline) {
            Ok(())
        } else {
            Err(why)
        }
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

/// The check at a publish point: every write `writes` noted has been
/// cleaned to memory. Counted either way, for stage 10's check.
///
/// # Errors
///
/// [`UNCLEANED_WHY`], for a write not cleaned.
fn require_published(writes: &Unpublished) -> Result<(), &'static str> {
    let _ = PUBLISHES_CHECKED.fetch_add(1, Ordering::Relaxed);
    if writes.is_published() {
        Ok(())
    } else {
        let _ = UNCLEANED.fetch_add(1, Ordering::Relaxed);
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
