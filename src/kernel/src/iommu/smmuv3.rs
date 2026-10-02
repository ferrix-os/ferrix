//! Arm `SMMUv3`, stage 2 only: a linear stream table, the command and event
//! queues, and the stage-2 tables of the domains built on it.
//!
//! Written against the specification and checked against QEMU's
//! `hw/arm/smmuv3.c` and `smmuv3-internal.h`, which the boot test runs. What
//! this does, and all it does:
//!
//! * **a linear stream table of 256 entries**, streams 0 to 255, which is every
//!   function on bus 0 and so every function QEMU's `virt` puts behind its
//!   SMMU. Every entry is valid and aborting until a domain is attached, so a
//!   stream with no domain reaches nothing;
//! * **stage-2 translation** for an attached stream: its domain's VMID, 39 bits
//!   of input address from level 1 over a 4 KiB granule, 40 bits of output,
//!   `AArch64` tables — `src/lib/kernel/paging`'s [`ArmStage2`] — and faults recorded;
//! * **the command queue**, for `CFGI_STE`, `TLBI_S12_VMALL` and `SYNC`,
//!   polled rather than signalled;
//! * **the event queue**, enabled so a fault is recorded, and read when the
//!   kernel asks: every event in it, of whatever type
//!   ([`Unit::take_fault`]).
//!
//! # Only a unit whose walk snoops
//!
//! The stream table, the queues and the stage-2 tables are written through
//! the cached direct map, and nothing here cleans them to memory. So a unit
//! is brought up only if `IDR0.COHACC` says its table and queue accesses are
//! coherent with the processors' caches, and only once `CR1` and each
//! entry's walk say those accesses are write-back cacheable and inner
//! shareable, as Linux's `arm_smmu_device_reset` and its stage-2 entries do.
//! A unit without `COHACC` is refused, not cleaned for: no machine this
//! kernel runs on has one (finding F-58).
//!
//! # Every domain maps the MSI doorbell
//!
//! QEMU sends a device's MSI writes through its stream like any other DMA; the
//! SMMU has no exemption for them, unlike VT-d. So every domain maps the
//! `GICv2m` frame's page at its own address, or the device's interrupts stop
//! the moment translation is on.

use alloc::collections::BTreeSet;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::coherence::Unpublished;
use ferrix_paging::stage2::ArmStage2;
use ferrix_paging::{MapError, MapFlags};
use ferrix_sync::IrqSpinLock;

use super::gate::{self, Gate};
use super::{Cause, Fault};
use crate::mmio::Mmio;
use crate::{arch, mm, timer, vmap};

/// Bytes of registers mapped: every register this driver uses is in the first
/// page of the unit's first register page.
const WINDOW: u64 = 0x1000;

/// Identification register 0.
const IDR0: u64 = 0x00;
/// Identification register 1.
const IDR1: u64 = 0x04;
/// Identification register 5.
const IDR5: u64 = 0x14;
/// Control register 0.
const CR0: u64 = 0x20;
/// Control register 0's acknowledgement.
const CR0ACK: u64 = 0x24;
/// Control register 1: the cacheability and shareability of the unit's
/// own accesses to its tables and queues.
const CR1: u64 = 0x28;
/// Control register 2.
const CR2: u64 = 0x2C;
/// Global errors.
const GERROR: u64 = 0x60;
/// Stream table base.
const STRTAB_BASE: u64 = 0x80;
/// Stream table format and size.
const STRTAB_BASE_CFG: u64 = 0x88;
/// Command queue base.
const CMDQ_BASE: u64 = 0x90;
/// Command queue producer index.
const CMDQ_PROD: u64 = 0x98;
/// Command queue consumer index.
const CMDQ_CONS: u64 = 0x9C;
/// Event queue base.
const EVENTQ_BASE: u64 = 0xA0;
/// Event queue producer index.
const EVENTQ_PROD: u64 = 0xA8;
/// Event queue consumer index.
const EVENTQ_CONS: u64 = 0xAC;
/// Bit 31 of both event queue indexes: `OVFLG` in the producer's, which the
/// unit toggles when it had an event to record and the queue was full, and
/// `OVACKFLG` in the consumer's, which software sets equal to it to say it
/// has seen that. Unequal, events were lost since the last acknowledgement.
const EVENTQ_OVERFLOW: u32 = 1 << 31;

/// IDR0: stage 2 is supported.
const IDR0_S2P: u32 = 1 << 0;
/// IDR0: the translation table formats, bits 3:2; bit 3 set means `AArch64`.
const IDR0_TTF_AARCH64: u32 = 1 << 3;
/// IDR0: the unit's accesses to its tables and queues are coherent with
/// the processors' caches.
const IDR0_COHACC: u32 = 1 << 4;
/// IDR0: VMIDs are sixteen bits rather than eight.
const IDR0_VMID16: u32 = 1 << 18;
/// IDR1: how many bits of stream ID the unit decodes, bits 5:0.
const IDR1_SIDSIZE: u32 = 0x3F;
/// IDR5: output address size, bits 2:0; 2 means 40 bits.
const IDR5_OAS: u32 = 0b111;
/// IDR5: a 4 KiB granule is supported.
const IDR5_GRAN4K: u32 = 1 << 4;

/// CR0: translation on.
const SMMUEN: u32 = 1 << 0;
/// CR0: the event queue is on.
const EVENTQEN: u32 = 1 << 2;
/// CR0: the command queue is on.
const CMDQEN: u32 = 1 << 3;
/// CR1: the stream table and the queues are read and written write-back
/// cacheable, inner and outer, and inner shareable: `TABLE_SH` and
/// `QUEUE_SH` 3, `TABLE_OC`, `TABLE_IC`, `QUEUE_OC` and `QUEUE_IC` 1.
const CR1_WALKS: u32 = 3 << 10 | 1 << 8 | 1 << 6 | 3 << 4 | 1 << 2 | 1;
/// CR2: record an event for a stream ID past the table.
const RECINVSID: u32 = 1 << 1;
/// GERROR: the command queue stopped on an error.
const GERROR_CMDQ_ERR: u32 = 1 << 0;
/// `CMDQ_CONS`: the error a stopped command left, bits 30:24.
const CMDQ_CONS_ERR: u32 = 0x7F << 24;

/// Streams the table covers, as bits: 256 entries.
const STREAM_BITS: u32 = 8;
/// Bytes of one stream table entry.
const STE_BYTES: u64 = 64;
/// Commands the queue holds, as bits: 256 of 16 bytes, one page.
const COMMAND_BITS: u32 = 8;
/// Bytes of one command.
const COMMAND_BYTES: u64 = 16;
/// Events the queue holds, as bits: 128 of 32 bytes, one page.
const EVENT_BITS: u32 = 7;
/// Bytes of one event record.
const EVENT_BYTES: u64 = 32;
/// Event type: a translation fault, the first of the four that refuse an
/// access at an address.
const EVENT_F_TRANSLATION: u8 = 0x10;
/// Event type: a permission fault, the last of those four, after an address
/// size fault (0x11) and an access flag fault (0x12).
const EVENT_F_PERMISSION: u8 = 0x13;
/// Every event type the architecture defines, by the name the specification
/// gives it (Arm IHI 0070, "Event records").
const EVENT_NAMES: [(u8, &str); 18] = [
    (0x01, "F_UUT"),
    (0x02, "C_BAD_STREAMID"),
    (0x03, "F_STE_FETCH"),
    (0x04, "C_BAD_STE"),
    (0x05, "F_BAD_ATS_TREQ"),
    (0x06, "F_STREAM_DISABLED"),
    (0x07, "F_TRANSL_FORBIDDEN"),
    (0x08, "C_BAD_SUBSTREAMID"),
    (0x09, "F_CD_FETCH"),
    (0x0A, "C_BAD_CD"),
    (0x0B, "F_WALK_EABT"),
    (0x10, "F_TRANSLATION"),
    (0x11, "F_ADDR_SIZE"),
    (0x12, "F_ACCESS"),
    (0x13, "F_PERMISSION"),
    (0x20, "F_TLB_CONFLICT"),
    (0x21, "F_CFG_CONFLICT"),
    (0x24, "E_PAGE_REQUEST"),
];
/// Event record word 3: the access was a read.
const EVENT_READ: u64 = 1 << 3;

/// STE word 0: valid.
const STE_VALID: u64 = 1;
/// STE word 0, `Config` in bits 3:1: stage 2 translates, stage 1 bypassed.
const STE_S2_TRANSLATE: u64 = 0b110 << 1;
/// STE word 5 for this driver's walk: `S2T0SZ` 25, `S2SL0` 1, the walk
/// write-back cacheable and inner shareable (`S2IR0` and `S2OR0` 1, `S2SH0`
/// 3), a 4 KiB `S2TG`, `S2PS` 2 for 40 bits, `S2AA64`, and `S2R` so faults
/// are recorded.
const STE_S2_WALK: u64 = 25 | 1 << 6 | 1 << 8 | 1 << 10 | 3 << 12 | 2 << 16 | 1 << 19 | 1 << 26;

/// Command: make the unit reread a stream table entry.
const CMD_CFGI_STE: u64 = 0x03;
/// Command: forget every cached stage-2 translation for a VMID.
const CMD_TLBI_S12_VMALL: u64 = 0x28;
/// Command: report once every command before it has been consumed.
const CMD_SYNC: u64 = 0x46;

/// How long a command or a control change may take.
const PATIENCE_NANOS: u64 = 100_000_000;

/// What a device's MSI doorbell is mapped with.
const DOORBELL: MapFlags = MapFlags {
    device: true,
    uncached: false,
    write_combining: false,
    ..MapFlags::DMA
};

/// One `SMMUv3`.
#[derive(Debug)]
pub(crate) struct Unit {
    /// Its registers.
    registers: Mmio,
    /// Physical address of the stream table.
    table: u64,
    /// Physical address of the command queue.
    commands: u64,
    /// Physical address of the event queue.
    events: u64,
    /// The largest VMID the unit takes.
    vmids: u32,
    /// The `GICv2m` doorbell page every domain maps, if the machine has one.
    doorbell: Option<u64>,
    /// VMIDs in use, and the command queue's producer index, together. Held
    /// only while they change, never across a wait.
    state: IrqSpinLock<State, arch::Irq>,
    /// Held across a command and its wait, so two cannot interleave. A gate,
    /// not a lock: the wait is made with interrupts on.
    commanding: Gate,
}

/// What a unit hands out and where its command queue stands.
#[derive(Debug, Default)]
struct State {
    /// VMIDs in use.
    vmids: BTreeSet<u16>,
    /// The next command slot, wrap bit included.
    produced: u32,
}

/// A domain on one unit: the stream its table entry names, its VMID, and the
/// root of its stage-2 tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Attached {
    /// The stream.
    stream: u32,
    /// The VMID.
    vmid: u16,
    /// Physical address of the stage-2 root table.
    root: u64,
}

impl Attached {
    /// The stream ID the unit sees the function as.
    pub(crate) fn stream(&self) -> u32 {
        self.stream
    }
}

impl Unit {
    /// Map the unit whose registers are at `phys`, and require it to be able to
    /// do what this driver asks.
    ///
    /// # Errors
    ///
    /// Why the unit is left alone.
    pub(crate) fn open(phys: u64, doorbell: Option<u64>) -> Result<Unit, &'static str> {
        let base =
            vmap::map_device(phys, WINDOW).map_err(|_| "its registers could not be mapped")?;
        let registers = Mmio::at(base);
        let refuse = |why: &'static str| -> Result<Unit, &'static str> {
            let _ = vmap::unmap_device(base);
            Err(why)
        };
        let idr0 = registers.read32(IDR0);
        if idr0 & IDR0_S2P == 0 || idr0 & IDR0_TTF_AARCH64 == 0 {
            return refuse("it has no AArch64 stage 2");
        }
        if idr0 & IDR0_COHACC == 0 {
            return refuse("its table and queue accesses do not snoop the caches");
        }
        if registers.read32(IDR1) & IDR1_SIDSIZE < STREAM_BITS {
            return refuse("it decodes fewer stream ID bits than the table covers");
        }
        let idr5 = registers.read32(IDR5);
        if idr5 & IDR5_GRAN4K == 0 || idr5 & IDR5_OAS < 2 {
            return refuse("it cannot walk 4 KiB tables with 40-bit output");
        }
        if registers.read32(CR0) & SMMUEN != 0 {
            return refuse("firmware left it translating");
        }
        let Some(table) = frames(2) else {
            return refuse("no frames for its stream table");
        };
        let Some(commands) = frames(0) else {
            return refuse("no frame for its command queue");
        };
        let Some(events) = frames(0) else {
            return refuse("no frame for its event queue");
        };
        // Every stream valid and aborting: a device with no domain reaches
        // nothing, rather than bypassing.
        for stream in 0..1_u64 << STREAM_BITS {
            write_entry(table + stream * STE_BYTES, STE_VALID);
        }
        let unit = Unit {
            registers,
            table,
            commands,
            events,
            vmids: if idr0 & IDR0_VMID16 != 0 {
                0xFFFF
            } else {
                0xFF
            },
            doorbell,
            state: IrqSpinLock::new(State::default()),
            commanding: Gate::new(),
        };
        unit.program(events)?;
        Ok(unit)
    }

    /// Point the unit at its table and queues, and turn the queues on.
    fn program(&self, events: u64) -> Result<(), &'static str> {
        self.control(0, "it never stopped")?;
        // Only while the queues and translation are off.
        self.registers.write32(CR1, CR1_WALKS);
        if self.registers.read32(CR1) != CR1_WALKS {
            return Err("it did not take cacheable walks of its table and queues");
        }
        write64(self.registers, STRTAB_BASE, self.table);
        self.registers.write32(STRTAB_BASE_CFG, STREAM_BITS);
        write64(
            self.registers,
            CMDQ_BASE,
            self.commands | u64::from(COMMAND_BITS),
        );
        self.registers.write32(CMDQ_PROD, 0);
        self.registers.write32(CMDQ_CONS, 0);
        write64(self.registers, EVENTQ_BASE, events | u64::from(EVENT_BITS));
        self.registers.write32(EVENTQ_PROD, 0);
        self.registers.write32(EVENTQ_CONS, 0);
        self.registers.write32(CR2, RECINVSID);
        self.control(CMDQEN | EVENTQEN, "it never started its queues")?;
        self.command(CMD_SYNC, 0)
    }

    /// Turn translation on. From here a stream with no domain reaches nothing.
    ///
    /// # Errors
    ///
    /// A unit that never acknowledged.
    pub(crate) fn enable(&self) -> Result<(), &'static str> {
        self.control(CMDQEN | EVENTQEN | SMMUEN, "it never started translating")
    }

    /// The next event in the queue, consumed, or `None` when the queue holds
    /// none.
    ///
    /// Every event comes back, whatever its type. A translation, address
    /// size, access flag or permission fault is a refused access at the
    /// address the record gives, [`Cause::Access`], as a VT-d fault record
    /// is; any other type is [`Cause::Event`], which names the stream but no
    /// access. This used to consume those unread, passing over every event
    /// but a translation fault, so a stream past the table or a stream table
    /// entry the unit refused would have gone unseen.
    ///
    /// An overflow comes back too, first, as [`Cause::Lost`], and is
    /// acknowledged in the same step: the queue was full and the unit dropped
    /// events it had to record. Each consumption keeps the acknowledgement it
    /// found, as Linux's `queue_inc_cons` keeps `Q_OVF`, so it is not undone.
    pub(crate) fn take_fault(&self) -> Option<Fault> {
        let index = (1_u32 << (EVENT_BITS + 1)) - 1;
        let producer = self.registers.read32(EVENTQ_PROD);
        let consumer = self.registers.read32(EVENTQ_CONS);
        let acknowledged = consumer & EVENTQ_OVERFLOW;
        let consumed = consumer & index;
        if (producer ^ consumer) & EVENTQ_OVERFLOW != 0 {
            self.registers
                .write32(EVENTQ_CONS, consumed | (producer & EVENTQ_OVERFLOW));
            return Some(Fault {
                stream: 0,
                page: 0,
                write: false,
                cause: Cause::Lost,
            });
        }
        let produced = producer & index;
        if produced == consumed {
            return None;
        }
        let slot = consumed & ((1 << EVENT_BITS) - 1);
        let at = self.events + u64::from(slot) * EVENT_BYTES;
        // Words 0 and 1: the type and the stream. Words 2 and 3: the
        // access. Words 4 and 5: the input address.
        let first = read_entry(at);
        let access = read_entry(at + 8);
        let address = read_entry(at + 16);
        self.registers
            .write32(EVENTQ_CONS, ((consumed + 1) & index) | acknowledged);
        let kind = (first & 0xFF) as u8;
        Some(Fault {
            stream: (first >> 32) as u32,
            page: address & !0xFFF,
            write: (access >> 32) & EVENT_READ == 0,
            cause: if (EVENT_F_TRANSLATION..=EVENT_F_PERMISSION).contains(&kind) {
                Cause::Access
            } else {
                Cause::Event(kind)
            },
        })
    }

    /// Give `stream` a domain of its own: an empty stage-2 tree with the MSI
    /// doorbell mapped, which its table entry points at.
    ///
    /// # Errors
    ///
    /// Why not: a stream past the table, one already attached, no VMID or no
    /// frame left, or a unit that never finished a command.
    pub(crate) fn attach(&self, stream: u32) -> Result<Attached, &'static str> {
        if stream >> STREAM_BITS != 0 {
            return Err("the stream is past the table this driver keeps");
        }
        let entry = self.table + u64::from(stream) * STE_BYTES;
        if read_entry(entry) & STE_S2_TRANSLATE != 0 {
            return Err("the stream already has a domain");
        }
        let root = frames(0).ok_or("no frame for a domain's tables")?;
        let vmid = {
            let mut state = self.state.lock();
            let vmid = (1..=self.vmids)
                .filter_map(|candidate| u16::try_from(candidate).ok())
                .find(|candidate| !state.vmids.contains(candidate));
            // No memory to record it is no VMID to give.
            vmid.filter(|&vmid| {
                crate::fallible::reserve().is_ok_and(|held| {
                    crate::fallible::insert_into_set_held(&held, &mut state.vmids, vmid)
                })
            })
        };
        let Some(vmid) = vmid else {
            mm::deallocate_frames(root / PAGE_SIZE, 0);
            return Err("the unit has no VMID left");
        };
        let attached = Attached { stream, vmid, root };
        if let Some(doorbell) = self.doorbell
            && self.map(&attached, doorbell, doorbell, DOORBELL).is_err()
        {
            self.release(attached);
            return Err("the MSI doorbell could not be mapped");
        }
        // The walk before the valid configuration, so the unit never reads a
        // translating entry with a stale table. An entry is sixteen 32-bit
        // words: words 4 and 5 are the VMID and the walk, words 6 and 7 the
        // table's address.
        write_entry(entry + 16, u64::from(vmid) | STE_S2_WALK << 32);
        write_entry(entry + 24, root);
        write_entry(entry, STE_VALID | STE_S2_TRANSLATE);
        self.command(CMD_CFGI_STE, u64::from(stream))?;
        Ok(attached)
    }

    /// Put `attached`'s stream back to aborting, make the unit forget it, and
    /// give back its tables. Every page but the doorbell must have been
    /// unmapped.
    ///
    /// # Errors
    ///
    /// A unit that never finished; the tables are then kept.
    pub(crate) fn detach(&self, attached: Attached) -> Result<(), &'static str> {
        let entry = self.table + u64::from(attached.stream) * STE_BYTES;
        write_entry(entry, STE_VALID);
        write_entry(entry + 16, 0);
        write_entry(entry + 24, 0);
        self.command(CMD_CFGI_STE, u64::from(attached.stream))?;
        self.command(CMD_TLBI_S12_VMALL, u64::from(attached.vmid))?;
        if let Some(doorbell) = self.doorbell {
            // The stream aborts and the unit has forgotten the VMID: nothing
            // walks these tables any more, so they go back at once.
            let mut tables = mm::UnlinkedTables::of(mm::TableOwner::Device);
            let _ = self.unmap(&attached, doorbell, &mut tables);
            tables.release();
        }
        self.release(attached);
        Ok(())
    }

    /// Give back `attached`'s VMID and root table.
    fn release(&self, attached: Attached) {
        let _ = self.state.lock().vmids.remove(&attached.vmid);
        mm::deallocate_frames(attached.root / PAGE_SIZE, 0);
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
        // A unit without COHACC was refused, so nothing needs cleaning.
        mm::map_io::<ArmStage2>(
            attached.root,
            iova,
            phys,
            flags,
            &mut Unpublished::new(true),
        )
    }

    /// Take the page at `iova` out of `attached`'s tables, holding every table
    /// that empties in `tables`. The unit may still reach the page, and walk
    /// the tables, until [`Unit::flush`] returns.
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
        mm::unmap_io::<ArmStage2>(attached.root, iova, tables, &mut Unpublished::new(true))
    }

    /// Where `attached`'s tables send an access to `iova`.
    pub(crate) fn resolve(&self, attached: &Attached, iova: u64) -> Option<u64> {
        mm::translate_io::<ArmStage2>(attached.root, iova)
    }

    /// Make the unit forget what it cached of `attached`'s tables. After a map
    /// there is nothing to forget: the unit caches only translations it
    /// found.
    ///
    /// # Errors
    ///
    /// A unit that never finished.
    pub(crate) fn flush(&self, attached: &Attached, after_map: bool) -> Result<(), &'static str> {
        if after_map {
            return Ok(());
        }
        self.command(CMD_TLBI_S12_VMALL, u64::from(attached.vmid))
    }

    /// Queue `opcode` with `operand` in the high half of its first word, then a
    /// `SYNC`, and wait until the unit has consumed both.
    fn command(&self, opcode: u64, operand: u64) -> Result<(), &'static str> {
        let _entered = self.commanding.enter()?;
        let first = [
            opcode | operand << 32,
            if opcode == CMD_CFGI_STE { 1 } else { 0 },
        ];
        let produced = {
            let mut state = self.state.lock();
            for words in [first, [CMD_SYNC, 0]] {
                let slot = state.produced & ((1 << COMMAND_BITS) - 1);
                let at = self.commands + u64::from(slot) * COMMAND_BYTES;
                write_entry(at, words[0]);
                write_entry(at + 8, words[1]);
                state.produced = (state.produced + 1) & ((1 << (COMMAND_BITS + 1)) - 1);
            }
            state.produced
        };
        self.registers.write32(CMDQ_PROD, produced);
        let index = (1 << (COMMAND_BITS + 1)) - 1;
        self.wait(
            || {
                let consumed = self.registers.read32(CMDQ_CONS);
                consumed & index == produced
                    || consumed & CMDQ_CONS_ERR != 0
                    || self.registers.read32(GERROR) & GERROR_CMDQ_ERR != 0
            },
            "it never finished a command",
        )?;
        if self.registers.read32(CMDQ_CONS) & CMDQ_CONS_ERR != 0
            || self.registers.read32(GERROR) & GERROR_CMDQ_ERR != 0
        {
            return Err("it refused a command");
        }
        Ok(())
    }

    /// Write `value` to CR0 and wait for CR0ACK to say the same.
    fn control(&self, value: u32, why: &'static str) -> Result<(), &'static str> {
        self.registers.write32(CR0, value);
        self.wait(|| self.registers.read32(CR0ACK) == value, why)
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

/// Zeroed frames, 2^`order` of them contiguous, by physical address.
fn frames(order: u8) -> Option<u64> {
    let first = mm::allocate_frames(order)?;
    for frame in first..first + (1 << order) {
        mm::zero_frame(frame);
    }
    Some(first * PAGE_SIZE)
}

/// Read the eight bytes at physical address `at`.
fn read_entry(at: u64) -> u64 {
    // SAFETY: (DMA) `at` is a whole, aligned eight bytes inside a stream table or
    // queue this unit took from the frame allocator for itself and never gave
    // back; the direct map covers every frame of RAM. The unit reads them too,
    // which is why the access is volatile.
    unsafe { core::ptr::read_volatile(mm::direct_map(at) as *const u64) }
}

/// Write the eight bytes at physical address `at`.
fn write_entry(at: u64, value: u64) {
    // SAFETY: (DMA) as `read_entry`: a whole, aligned eight bytes in memory only
    // this unit's code writes.
    unsafe { core::ptr::write_volatile(mm::direct_map(at) as *mut u64, value) };
}

/// The specification's name for event type `kind`, or `"reserved"`.
pub(crate) fn event_name(kind: u8) -> &'static str {
    EVENT_NAMES
        .iter()
        .find(|&&(number, _)| number == kind)
        .map_or("reserved", |&(_, name)| name)
}

/// Read a 64-bit register as two 32-bit halves, low first.
#[expect(
    dead_code,
    reason = "the event queue is read by the out-of-domain check"
)]
fn read64(registers: Mmio, at: u64) -> u64 {
    u64::from(registers.read32(at)) | u64::from(registers.read32(at + 4)) << 32
}

/// Write a 64-bit register as two 32-bit halves, low first.
fn write64(registers: Mmio, at: u64, value: u64) {
    registers.write32(at, value as u32);
    registers.write32(at + 4, (value >> 32) as u32);
}
