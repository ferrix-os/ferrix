//! AMD-Vi: one AMD IOMMU unit, its device table, the v1 tables of the
//! domains built on it, its command buffer and event log, and each
//! function's interrupt remapping table (`docs/AMD-VI.md`).
//!
//! Written against the AMD I/O Virtualization Technology (IOMMU)
//! Specification, #48882, and checked against QEMU 10.2's
//! `hw/i386/amd_iommu.c`, which the AMD-Vi boot row runs, and Linux's
//! `drivers/iommu/amd`. The encodings and the decisions that need no
//! hardware are `ferrix_paging::amdvi`'s, host-tested there.
//!
//! # The device table, blocked
//!
//! One table of 65,536 entries (2 MiB), every entry written blocked before
//! the unit is enabled: on a translating unit `Mode` 3 over one empty root
//! with `IR` and `IW` clear and domain 0, which QEMU enforces as hardware
//! does; on a unit that only remaps, untranslated. Every entry's interrupt
//! half is `IV` set with `IntCtl` abort, so nothing passes unremapped. An
//! attach writes the DMA half, a function's first vector the interrupt
//! half, each under `tables` and neither touching the other's words
//! (`ferrix_paging::amdvi::table`).
//!
//! # Commands, waited for by sequence
//!
//! Every invalidation goes through the command buffer, followed by a
//! `COMPLETION_WAIT` that stores a sequence number of the unit's own,
//! which the kernel waits for up to [`PATIENCE_NANOS`]. A wait that is not
//! stored in time, or a buffer the unit stopped running, fails: the unit is
//! then marked failed, no command is issued on it again and nothing is
//! released from its domains until reboot, as a VT-d unit whose queue
//! stopped is.
//!
//! # The unit snoops
//!
//! `Control.Coherent` is set and AMD's units snoop their own table reads,
//! so nothing is cleaned to memory: every write goes through an
//! [`Unpublished`] record that is coherent, so the mapper's discipline is
//! the same as for VT-d's units with `ECAP.C` set.
//!
//! # Faults
//!
//! The event log's interrupt is off; [`Unit::take_fault`] reads it, where
//! VT-d's fault records are read.

use alloc::collections::{BTreeMap, BTreeSet};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_acpi::ivrs::{Entry as IvhdEntry, Ivhd};
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::amdvi::AmdV1;
use ferrix_paging::amdvi::command::{self, Command, Completion};
use ferrix_paging::amdvi::event::{self, Event};
use ferrix_paging::amdvi::table;
use ferrix_paging::amdvi::unit::{self as policy, Accepted};
use ferrix_paging::coherence::Unpublished;
use ferrix_paging::{MapError, MapFlags};
use ferrix_pci::Address;
use ferrix_sync::IrqSpinLock;

use super::gate::{self, Gate};
use super::{Cause, Fault, Wait};
use crate::mmio::Mmio;
use crate::{arch, mm, timer, vmap};

/// How long a completion wait is given.
const PATIENCE_NANOS: u64 = 100_000_000;

/// How long a wait that stores nothing is given before it fails: check R7.
const UNWRITTEN_PATIENCE_NANOS: u64 = 2_000_000;

/// The order of the device table's block: 512 frames, 65,536 entries.
const DEVICE_TABLE_ORDER: u8 = 9;

/// Entries in the device table.
const DEVICE_ENTRIES: u32 = 0x1_0000;

/// Bytes of a device table entry.
const DEVICE_ENTRY_BYTES: u64 = 32;

/// The fault reason an AMD-Vi unit's refused interrupt request is recorded
/// with, outside VT-d's 0x20 to 0x26: the unit names no reason of its own.
pub(crate) const INTERRUPT_REFUSED: u8 = 0x80;

/// Why every command on a failed unit fails at once.
const FAILED_WHY: &str = "its command buffer stopped or a wait was never stored, so it takes no \
                          command again";

/// Commands issued and waited for, every unit.
static COMMANDS: AtomicU64 = AtomicU64::new(0);
/// Completion waits stored as asked.
static WAITS: AtomicU64 = AtomicU64::new(0);
/// Waits that failed.
static WAITS_FAILED: AtomicU64 = AtomicU64::new(0);

/// What the AMD-Vi units' commands came to, for stage 10's check.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Commands {
    /// Commands issued.
    pub(crate) issued: u64,
    /// Completion waits stored as asked.
    pub(crate) waited: u64,
    /// Waits that failed.
    pub(crate) failed: u64,
}

/// The counters so far.
pub(crate) fn commands() -> Commands {
    Commands {
        issued: COMMANDS.load(Ordering::Relaxed),
        waited: WAITS.load(Ordering::Relaxed),
        failed: WAITS_FAILED.load(Ordering::Relaxed),
    }
}

/// One AMD-Vi unit.
#[derive(Debug)]
pub struct Unit {
    /// Physical address of its registers, which names it on the console.
    phys: u64,
    /// Its own PCI function's devid.
    devid: u16,
    /// Its registers.
    registers: Mmio,
    /// Physical address of the device table.
    device_table: u64,
    /// Physical address of the empty table every blocked entry of a
    /// translating unit points at; 0 on a unit that only remaps.
    empty_root: u64,
    /// Physical address of the command buffer's frame.
    commands: u64,
    /// Physical address of the event log's frame.
    events: u64,
    /// Physical address of the word completion waits store to.
    status: u64,
    /// The command slot the next command goes in. Changed only under `gate`.
    tail: AtomicU32,
    /// The last wait's sequence number. Changed only under `gate`.
    sequence: AtomicU64,
    /// Whether the unit takes no command again.
    failed: AtomicBool,
    /// Whether the end-of-boot audit has been told of the failure.
    failure_reported: AtomicBool,
    /// Whether its domains translate: false for a unit that only remaps.
    translates: bool,
    /// Whether it takes `INVALIDATE_IOMMU_ALL`.
    invalidate_all: bool,
    /// Whether it is enabled.
    enabled: AtomicBool,
    /// Whether its entries remap interrupts: false once
    /// [`Unit::leave_interrupts_alone`] has cleared every `IV`.
    interrupts: AtomicBool,
    /// The tables' bookkeeping, and the lock both halves of every device
    /// table entry are written under.
    tables: IrqSpinLock<Tables, arch::Irq>,
    /// Held across reading one event.
    taken: IrqSpinLock<(), arch::Irq>,
    /// Held across a batch of commands and its wait.
    gate: Gate,
    /// Held across a function's first vector, so its table is made once.
    remaps: Gate,
}

/// What a unit has handed out.
#[derive(Debug, Default)]
struct Tables {
    /// Domain IDs in use; 0 is the blocked entries' and never handed out.
    identifiers: BTreeSet<u16>,
    /// Each function's interrupt remapping table, by devid.
    interrupts: BTreeMap<u16, u64>,
}

/// A domain on one unit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Attached {
    /// The function's devid.
    devid: u16,
    /// The domain ID.
    identifier: u16,
    /// Physical address of the root table.
    root: u64,
}

impl Attached {
    /// The devid the unit sees the function as.
    pub(crate) fn stream(&self) -> u32 {
        u32::from(self.devid)
    }
}

impl Unit {
    /// Map the unit `ivhd` describes, decide whether the kernel drives it
    /// (`ferrix_paging::amdvi::unit::accept`), stop it if firmware left it
    /// on, and make its tables: the device table with every entry blocked,
    /// the command buffer, the event log and the completion word. Nothing
    /// is enabled yet. With `held`, an IVMD holds the unit to remapping
    /// only: it does not translate, whatever it could.
    ///
    /// # Errors
    ///
    /// Why the unit is left alone.
    pub(crate) fn open(ivhd: &Ivhd<'_>, held: bool) -> Result<Unit, &'static str> {
        let base = vmap::map_device(ivhd.base, policy::WINDOW)
            .map_err(|_| "its registers could not be mapped")?;
        let registers = Mmio::at(base);
        let refuse = |why: &'static str| -> Result<Unit, &'static str> {
            let _ = vmap::unmap_device(base);
            Err(why)
        };
        let efr = ivhd
            .efr
            .unwrap_or_else(|| read64(registers, policy::EXTENDED_FEATURES));
        let Accepted {
            translates,
            invalidate_all,
        } = match policy::accept(efr, ivhd.translation_disabled()) {
            Ok(accepted) => accepted,
            Err(why) => {
                crate::println!(
                    "  iommu    amd-vi unit {:#x}: EFR {efr:#x}: left alone: {why}",
                    ivhd.base
                );
                return refuse(why);
            }
        };
        let translates = translates && !held;
        if let Err(why) = stop_firmware(registers) {
            return refuse(why);
        }
        let Some(device_table) = mm::allocate_frames(DEVICE_TABLE_ORDER) else {
            return refuse("no 2 MiB block for its device table");
        };
        for frame in device_table..device_table + (1 << DEVICE_TABLE_ORDER) {
            mm::zero_frame(frame);
        }
        let Some([empty_root, commands, events, status]) = frames(translates) else {
            mm::deallocate_frames(device_table, DEVICE_TABLE_ORDER);
            return refuse("no frames for its tables");
        };
        let unit = Unit {
            phys: ivhd.base,
            devid: ivhd.devid,
            registers,
            device_table: device_table * PAGE_SIZE,
            empty_root,
            commands,
            events,
            status,
            tail: AtomicU32::new(0),
            sequence: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            failure_reported: AtomicBool::new(false),
            translates,
            invalidate_all,
            enabled: AtomicBool::new(false),
            interrupts: AtomicBool::new(true),
            tables: IrqSpinLock::new(Tables::default()),
            taken: IrqSpinLock::new(()),
            gate: Gate::new(),
            remaps: Gate::new(),
        };
        let blocked = table::blocked(unit.blocked_dma());
        for devid in 0..DEVICE_ENTRIES {
            unit.write_entry(devid as u16, blocked);
        }
        Ok(unit)
    }

    /// The DMA half every unattached entry holds.
    pub(crate) fn blocked_dma(&self) -> [u64; 2] {
        if self.translates {
            table::blocked_translating(self.empty_root)
        } else {
            table::untranslated()
        }
    }

    /// Point the unit at its tables, turn it on, and make it forget every
    /// entry it may have cached: `INVALIDATE_IOMMU_ALL` where it takes it,
    /// and `INVALIDATE_DEVTAB_ENTRY` and `INVALIDATE_INTERRUPT_TABLE` for
    /// every devid `entries` name and its own -- QEMU switches a function to
    /// translation only on its own devid's invalidation (ledger 3867,
    /// condition 1). From here every function reaches only what an entry
    /// allows, and raises only what its table remaps.
    ///
    /// # Errors
    ///
    /// The unit never ran its buffer, or a wait failed.
    pub(crate) fn enable(
        &self,
        entries: impl Iterator<Item = IvhdEntry>,
    ) -> Result<(), &'static str> {
        write64(
            self.registers,
            policy::DEVICE_TABLE,
            command::device_table_base(self.device_table),
        );
        write64(
            self.registers,
            policy::COMMAND_BASE,
            command::ring_base(self.commands),
        );
        write64(
            self.registers,
            policy::EVENT_BASE,
            command::ring_base(self.events),
        );
        write64(self.registers, policy::COMMAND_HEAD, 0);
        write64(self.registers, policy::COMMAND_TAIL, 0);
        write64(self.registers, policy::EVENT_HEAD, 0);
        write64(self.registers, policy::EVENT_TAIL, 0);
        // The hardware exclusion range off: every device with `EX` would
        // bypass translation in it (L.iommu.96).
        write64(self.registers, policy::EXCLUSION_BASE, 0);
        write64(self.registers, policy::EXCLUSION_LIMIT, 0);
        write64(self.registers, policy::CONTROL, policy::CONTROL_STANDING);
        wait_for(
            || read64(self.registers, policy::STATUS) & policy::STATUS_COMMANDS_RUNNING != 0,
            "it never ran its command buffer",
        )?;
        if self.invalidate_all {
            self.submit(&[command::invalidate_all()], Wait::Status)?;
        }
        let mut batch: alloc::vec::Vec<Command> =
            crate::fallible::try_with_capacity(BATCH).map_err(|_| "no memory for a batch")?;
        let own = core::iter::once((self.devid, self.devid));
        let named = entries.filter_map(|entry| match entry {
            IvhdEntry::Devices { first, last } | IvhdEntry::Alias { first, last, .. } => {
                Some((first, last))
            }
            IvhdEntry::Special { devid, .. } | IvhdEntry::AcpiHid { devid } => Some((devid, devid)),
            IvhdEntry::All => Some((0, u16::MAX)),
            IvhdEntry::Unreadable => None,
        });
        for (first, last) in own.chain(named) {
            for devid in first..=last {
                if batch.len() + 2 > BATCH {
                    self.submit(&batch, Wait::Status)?;
                    batch.clear();
                }
                let _ = crate::fallible::push_within(
                    &mut batch,
                    command::invalidate_device_entry(devid),
                );
                let _ = crate::fallible::push_within(
                    &mut batch,
                    command::invalidate_interrupt_table(devid),
                );
            }
        }
        if !batch.is_empty() {
            self.submit(&batch, Wait::Status)?;
        }
        self.enabled.store(true, Ordering::Release);
        Ok(())
    }

    /// Leave every function's interrupts unremapped: `IV` clear in every
    /// entry, before the unit is enabled. For a machine whose console line
    /// cannot be converted (`remapping::bring_up_amdvi`), as VT-d leaves a
    /// unit's remapping off; the machine's interrupts are then not
    /// isolated, and its line says so.
    pub(crate) fn leave_interrupts_alone(&self) {
        if self.enabled() {
            return;
        }
        self.interrupts.store(false, Ordering::Release);
        for devid in 0..DEVICE_ENTRIES {
            self.write_interrupt_half(devid as u16, 0);
        }
    }

    /// Give `function` a domain of its own holding exactly `regions`' pages,
    /// each at its own address and as `flags` allow, and nothing else: an
    /// IVMD's identity domain (`docs/AMD-VI.md` §4.1), made before the unit
    /// is enabled, or by stage 10's check on a running unit, and never
    /// detached (K2's rule, ledger 2388 condition 2).
    ///
    /// # Errors
    ///
    /// [`Unit::attach`]'s reasons, or a page the tables would not take; what
    /// was made is kept, since nothing is released on a unit that may have
    /// walked it.
    pub(crate) fn attach_identity(
        &self,
        function: Address,
        regions: &[ferrix_paging::vtd::rmrr::Identity],
        flags: MapFlags,
    ) -> Result<Attached, &'static str> {
        if self.enabled() && !crate::boot_checks::run() {
            return Err("an IVMD identity domain is made before its unit is enabled");
        }
        let attached = self.attach_blocked(function)?;
        for page in regions.iter().flat_map(|region| region.page_addresses()) {
            if flags.read || flags.write {
                self.map(&attached, page, page, flags)
                    .map_err(|_| "an IVMD page could not be mapped")?;
            }
        }
        if self.enabled() {
            self.flush(&attached, Wait::Status)?;
        }
        Ok(attached)
    }

    /// Physical address of its registers.
    pub(crate) fn phys(&self) -> u64 {
        self.phys
    }

    /// Its own function's devid.
    pub(crate) fn devid(&self) -> u16 {
        self.devid
    }

    /// Whether its domains translate.
    pub(crate) fn translating(&self) -> bool {
        self.translates && self.enabled()
    }

    /// Whether it is enabled.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Whether it is on, remapping every function's interrupts through its
    /// own table or aborting them.
    pub(crate) fn remapping(&self) -> bool {
        self.enabled() && self.interrupts.load(Ordering::Acquire)
    }

    /// The exclusion registers, as read back: base and limit (L.iommu.96).
    pub(crate) fn exclusion(&self) -> (u64, u64) {
        (
            read64(self.registers, policy::EXCLUSION_BASE),
            read64(self.registers, policy::EXCLUSION_LIMIT),
        )
    }

    /// The entry of `devid`, as the unit reads it.
    pub(crate) fn device_entry(&self, devid: u16) -> table::Entry {
        let at = self.device_table + u64::from(devid) * DEVICE_ENTRY_BYTES;
        [
            read_word(at),
            read_word(at + 8),
            read_word(at + 16),
            read_word(at + 24),
        ]
    }

    /// Write the whole entry of `devid`: only before the unit is enabled.
    fn write_entry(&self, devid: u16, entry: table::Entry) {
        let at = self.device_table + u64::from(devid) * DEVICE_ENTRY_BYTES;
        for (index, word) in entry.into_iter().enumerate() {
            write_word(at + index as u64 * 8, word);
        }
    }

    /// Write the DMA half of `devid`'s entry, quadword 1 (the domain) before
    /// quadword 0 (valid, the root), so it is never valid with a stale
    /// domain; under `tables`.
    fn write_dma_half(&self, devid: u16, dma: [u64; 2]) {
        let at = self.device_table + u64::from(devid) * DEVICE_ENTRY_BYTES;
        write_word(at + 8, dma[1]);
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        write_word(at, dma[0]);
    }

    /// Write the interrupt half of `devid`'s entry; under `tables`.
    fn write_interrupt_half(&self, devid: u16, interrupts: u64) {
        let at = self.device_table + u64::from(devid) * DEVICE_ENTRY_BYTES;
        write_word(at + 16, interrupts);
    }

    /// Give `function` a domain of its own: an empty three-level tree its
    /// entry points at, under a fresh domain ID.
    ///
    /// # Errors
    ///
    /// No frame, no domain ID left, a function already attached, or a wait
    /// that failed (the entry and its root are then kept).
    pub(crate) fn attach(&self, function: Address) -> Result<Attached, &'static str> {
        self.attach_blocked(function)
    }

    /// [`Unit::attach`]'s work: before the unit is enabled nothing it could
    /// have cached is invalidated, since enabling it invalidates every
    /// entry.
    fn attach_blocked(&self, function: Address) -> Result<Attached, &'static str> {
        if !self.translates {
            return Err("the unit only remaps interrupts");
        }
        let devid = function.requester_id();
        let frame = mm::allocate_frames(0).ok_or("no frame for a domain's tables")?;
        mm::zero_frame(frame);
        let root = frame * PAGE_SIZE;
        let installed = (|| {
            let mut tables = self.tables.lock();
            let held = crate::fallible::reserve().map_err(|_| "no memory to record a domain")?;
            if table::dma_half(&self.device_entry(devid)) != self.blocked_dma() {
                return Err("the function already has a domain");
            }
            let identifier = (1..=u16::MAX)
                .find(|candidate| !tables.identifiers.contains(candidate))
                .ok_or("the unit has no domain identifier left")?;
            let _ =
                crate::fallible::insert_into_set_held(&held, &mut tables.identifiers, identifier);
            self.write_dma_half(devid, table::attached(root, identifier));
            Ok(identifier)
        })();
        let identifier = match installed {
            Ok(identifier) => identifier,
            Err(why) => {
                mm::deallocate_frames(frame, 0);
                return Err(why);
            }
        };
        if self.enabled() {
            self.submit(&[command::invalidate_device_entry(devid)], Wait::Status)?;
        }
        Ok(Attached {
            devid,
            identifier,
            root,
        })
    }

    /// Block `attached`'s entry again, make the unit forget it and the
    /// domain's pages, and give back its root. Every page must have been
    /// unmapped, which gave back every table below the root.
    ///
    /// # Errors
    ///
    /// A wait that failed; the root is then kept, and so is the domain
    /// identifier, which the unit may still hold cached translations under.
    pub(crate) fn detach(&self, attached: Attached) -> Result<(), &'static str> {
        {
            let _tables = self.tables.lock();
            self.write_dma_half(attached.devid, self.blocked_dma());
        }
        self.submit(
            &[
                command::invalidate_device_entry(attached.devid),
                command::invalidate_domain(attached.identifier),
            ],
            Wait::Status,
        )?;
        let _ = self.tables.lock().identifiers.remove(&attached.identifier);
        mm::deallocate_frames(attached.root / PAGE_SIZE, 0);
        Ok(())
    }

    /// Map the page at `phys` at `iova` in `attached`'s tables.
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
        let mut writes = Unpublished::new(true);
        mm::map_io::<AmdV1>(attached.root, iova, phys, flags, &mut writes)
    }

    /// Take the page at `iova` out of `attached`'s tables. The unit may
    /// still reach it until [`Unit::flush`] returns.
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
        let mut writes = Unpublished::new(true);
        mm::unmap_io::<AmdV1>(attached.root, iova, tables, &mut writes)
    }

    /// Where `attached`'s tables send an access to `iova`.
    pub(crate) fn resolve(&self, attached: &Attached, iova: u64) -> Option<u64> {
        mm::translate_io::<AmdV1>(attached.root, iova)
    }

    /// Make the unit forget what it cached of `attached`'s tables: after an
    /// unmap, and after a map too, since a unit may cache an entry that was
    /// not present (`NpCache`, L.iommu.89).
    ///
    /// # Errors
    ///
    /// A wait that failed.
    pub(crate) fn flush(&self, attached: &Attached, wait: Wait) -> Result<(), &'static str> {
        self.submit(&[command::invalidate_domain(attached.identifier)], wait)
    }

    /// Write entry `index` of `devid`'s interrupt remapping table to deliver
    /// `vector` to `destination`, making the table on the function's first
    /// vector, and make the unit forget any copy before this returns: the
    /// entry is written and its table invalidated before the function's
    /// entry is pointed at a new table (ledger 3867, condition 4).
    ///
    /// # Errors
    ///
    /// The unit does not remap, an index past the table, an entry in use, no
    /// frame or memory for the table, or a wait that failed.
    pub(crate) fn remap(
        &self,
        devid: u16,
        index: u16,
        vector: u8,
        destination: u8,
    ) -> Result<(), &'static str> {
        if !self.remapping() {
            return Err("the unit does not remap");
        }
        if index >= table::TABLE_ENTRIES {
            return Err("an interrupt entry index past the function's table");
        }
        let _one = self.remaps.enter()?;
        let existing = self.tables.lock().interrupts.get(&devid).copied();
        let (irt, fresh) = match existing {
            Some(irt) => (irt, false),
            None => {
                let frame = mm::allocate_frames(0).ok_or("no frame for an interrupt table")?;
                mm::zero_frame(frame);
                (frame * PAGE_SIZE, true)
            }
        };
        let at = irt + u64::from(index) * 4;
        if table::interrupt_entry_used(read_half(at)) {
            return Err("an interrupt entry in use is never rewritten");
        }
        write_half(at, table::interrupt_entry(vector, destination));
        // On failure a fresh table nothing names yet is not freed: the unit
        // may have read it.
        self.submit(&[command::invalidate_interrupt_table(devid)], Wait::Status)?;
        if fresh {
            {
                let mut tables = self.tables.lock();
                let _ = crate::fallible::insert(&mut tables.interrupts, devid, irt)
                    .map_err(|_| "no memory to record an interrupt table")?;
                self.write_interrupt_half(devid, table::remapped(irt));
            }
            self.submit(&[command::invalidate_device_entry(devid)], Wait::Status)?;
        }
        Ok(())
    }

    /// The next event the unit logged, cleared so the slot can be reused,
    /// or `None`. A failed unit is reported once, first; an overflowed log
    /// once, as lost, and the log restarted.
    pub(crate) fn take_fault(&self) -> Option<Fault> {
        let _taken = self.taken.lock();
        if self.failed.load(Ordering::Acquire)
            && !self.failure_reported.swap(true, Ordering::AcqRel)
        {
            return Some(Fault {
                stream: u32::MAX,
                page: 0,
                write: false,
                cause: Cause::AmdViFailed,
            });
        }
        let head = command::slot_of(read64(self.registers, policy::EVENT_HEAD));
        let tail = command::slot_of(read64(self.registers, policy::EVENT_TAIL));
        if head != tail {
            let at = self.events + u64::from(head) * command::ENTRY_BYTES;
            let record = [read_word(at), read_word(at + 8)];
            write_word(at, 0);
            write_word(at + 8, 0);
            let next = (head + 1) % command::RING_ENTRIES;
            write64(
                self.registers,
                policy::EVENT_HEAD,
                command::slot_register(next),
            );
            return Some(fault_of(event::decode(record)));
        }
        if read64(self.registers, policy::STATUS) & policy::STATUS_EVENT_OVERFLOW == 0 {
            return None;
        }
        // Write one to clear, then the log restarted, as Linux's
        // `amd_iommu_restart_event_logging` does.
        write64(
            self.registers,
            policy::STATUS,
            policy::STATUS_EVENT_OVERFLOW,
        );
        write64(
            self.registers,
            policy::CONTROL,
            policy::CONTROL_STANDING & !policy::CONTROL_EVENT_LOG,
        );
        write64(self.registers, policy::CONTROL, policy::CONTROL_STANDING);
        Some(Fault {
            stream: u32::MAX,
            page: 0,
            write: false,
            cause: Cause::AmdViLost,
        })
    }

    /// Put `commands` and a completion wait behind them in the buffer, move
    /// the tail past them, and wait as `wait` says.
    ///
    /// # Errors
    ///
    /// A failed unit, no room, a wait not stored in time or a buffer the
    /// unit stopped -- either of which marks it failed.
    fn submit(&self, commands: &[Command], wait: Wait) -> Result<(), &'static str> {
        let result = self.submit_and_wait(commands, wait);
        if result.is_err() {
            let _ = WAITS_FAILED.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// [`Unit::submit`]'s work.
    fn submit_and_wait(&self, commands: &[Command], wait: Wait) -> Result<(), &'static str> {
        if self.failed.load(Ordering::Acquire) {
            return Err(FAILED_WHY);
        }
        let _held = self.gate.enter()?;
        let mut tail = self.tail.load(Ordering::Relaxed);
        let head = command::slot_of(read64(self.registers, policy::COMMAND_HEAD));
        let needed = u32::try_from(commands.len() + 1).unwrap_or(u32::MAX);
        if command::free_slots(head, tail) < needed {
            return Err("its command buffer had no room");
        }
        let sequence = self.sequence.load(Ordering::Relaxed).wrapping_add(1).max(1);
        self.sequence.store(sequence, Ordering::Relaxed);
        let completion = match wait {
            Wait::Status => Completion::Store {
                address: self.status,
                data: sequence,
            },
            Wait::Unwritten => Completion::Unwritten,
        };
        for written in commands
            .iter()
            .copied()
            .chain(core::iter::once(command::completion_wait(completion)))
        {
            let at = self.commands + u64::from(tail) * command::ENTRY_BYTES;
            write_word(at, written[0]);
            write_word(at + 8, written[1]);
            tail = (tail + 1) % command::RING_ENTRIES;
        }
        self.tail.store(tail, Ordering::Relaxed);
        let _ = COMMANDS.fetch_add(commands.len() as u64, Ordering::Relaxed);
        write64(
            self.registers,
            policy::COMMAND_TAIL,
            command::slot_register(tail),
        );
        let patience = match wait {
            Wait::Status => PATIENCE_NANOS,
            Wait::Unwritten => UNWRITTEN_PATIENCE_NANOS,
        };
        let deadline = timer::now_nanos().saturating_add(patience);
        let stopped =
            || read64(self.registers, policy::STATUS) & policy::STATUS_COMMANDS_RUNNING == 0;
        let _ = gate::poll(|| read_word(self.status) == sequence || stopped(), deadline);
        if read_word(self.status) == sequence {
            let _ = WAITS.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if wait == Wait::Unwritten && !stopped() {
            // Check R7's wait: it was never going to be stored. The buffer
            // still runs, so the unit is not failed; the caller keeps what it
            // would have released.
            return Err("it never finished a command");
        }
        self.fail();
        Err(FAILED_WHY)
    }

    /// Mark the unit failed, and say so once.
    fn fail(&self) {
        if self.failed.swap(true, Ordering::AcqRel) {
            return;
        }
        crate::println!(
            "  iommu    amd-vi unit {:#x}: a completion wait was never stored or its command \
             buffer stopped: it takes no command again, and nothing it may reach is released \
             until reboot",
            self.phys
        );
    }
}

/// How many commands a batch carries before its wait: the ring's slots,
/// less the wait's, less one always left empty.
const BATCH: usize = 252;

/// The fault an event is, for `iommu`'s bookkeeping.
fn fault_of(event: Event) -> Fault {
    match event {
        Event::Access { devid, page, write } => Fault {
            stream: u32::from(devid),
            page,
            write,
            cause: Cause::Access,
        },
        Event::Interrupt { devid } => Fault {
            stream: u32::from(devid),
            page: 0,
            write: false,
            cause: Cause::Interrupt {
                reason: INTERRUPT_REFUSED,
                index: 0,
            },
        },
        Event::Other { devid, code } => Fault {
            stream: u32::from(devid),
            page: 0,
            write: false,
            cause: Cause::AmdVi(code),
        },
    }
}

/// Turn off a unit firmware left on, and wait until its buffer and log say
/// they have stopped (L.iommu.83).
fn stop_firmware(registers: Mmio) -> Result<(), &'static str> {
    if !policy::left_on(read64(registers, policy::CONTROL)) {
        return Ok(());
    }
    crate::println!("  iommu    amd-vi: firmware left a unit on; it is turned off first");
    write64(registers, policy::CONTROL, 0);
    wait_for(
        || {
            read64(registers, policy::STATUS)
                & (policy::STATUS_COMMANDS_RUNNING | policy::STATUS_EVENT_RUNNING)
                == 0
        },
        "firmware left it on, and it would not stop",
    )
}

/// The empty root (0 on a unit that only remaps), the command buffer's,
/// the event log's and the completion word's frames, zeroed; or `None`,
/// with every frame taken given back.
fn frames(translates: bool) -> Option<[u64; 4]> {
    let mut taken = [0_u64; 4];
    for (slot, frame) in taken.iter_mut().enumerate() {
        if slot == 0 && !translates {
            continue;
        }
        let Some(fresh) = mm::allocate_frames(0) else {
            for &given in taken.iter().filter(|&&given| given != 0) {
                mm::deallocate_frames(given / PAGE_SIZE, 0);
            }
            return None;
        };
        mm::zero_frame(fresh);
        *frame = fresh * PAGE_SIZE;
    }
    Some(taken)
}

/// Wait for `ready`, up to the unit's patience.
fn wait_for(ready: impl Fn() -> bool, why: &'static str) -> Result<(), &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if gate::poll(ready, deadline) {
        Ok(())
    } else {
        Err(why)
    }
}

/// Read the eight bytes at physical address `at`.
fn read_word(at: u64) -> u64 {
    // SAFETY: (DMA) `at` is an eight-byte-aligned word in a frame this unit
    // took from the frame allocator for itself and never gave back -- its
    // device table, an interrupt table, its command buffer, its event log
    // or its completion word -- and the direct map covers every frame of
    // RAM. The unit reads or writes it too, which is why the access is
    // volatile.
    unsafe { core::ptr::read_volatile(mm::direct_map(at) as *const u64) }
}

/// Write the eight bytes at physical address `at`.
fn write_word(at: u64, value: u64) {
    // SAFETY: (DMA) as `read_word`: a whole, aligned word in a frame only
    // this unit's code writes.
    unsafe { core::ptr::write_volatile(mm::direct_map(at) as *mut u64, value) };
}

/// Read the four bytes at physical address `at`: an interrupt table entry.
fn read_half(at: u64) -> u32 {
    // SAFETY: (DMA) as `read_word`, for a four-byte-aligned entry of an
    // interrupt table this unit made.
    unsafe { core::ptr::read_volatile(mm::direct_map(at) as *const u32) }
}

/// Write the four bytes at physical address `at`: an interrupt table entry.
fn write_half(at: u64, value: u32) {
    // SAFETY: (DMA) as `read_half`.
    unsafe { core::ptr::write_volatile(mm::direct_map(at) as *mut u32, value) };
}

/// Read a 64-bit register as two 32-bit halves, low first.
fn read64(registers: Mmio, at: u64) -> u64 {
    u64::from(registers.read32(at)) | u64::from(registers.read32(at + 4)) << 32
}

/// Write a 64-bit register as two 32-bit halves, low first: QEMU takes a
/// base register whole when its high half is written.
fn write64(registers: Mmio, at: u64, value: u64) {
    registers.write32(at, value as u32);
    registers.write32(at + 4, (value >> 32) as u32);
}
