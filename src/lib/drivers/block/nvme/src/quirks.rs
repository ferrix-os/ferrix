//! What particular controllers need that the specification does not say.
//!
//! The facts are the ones Linux's NVMe driver records for these devices
//! (`drivers/nvme/host/pci.c`'s device table and `core.c`'s model table, as
//! of Linux 6.18); the table and the code are this crate's own. Only the
//! quirks that change something this driver does are here. The others
//! concern features it never uses and so cannot trip over:
//!
//! * Write Zeroes, Dataset Management, namespace identifiers, the
//!   subsystem NQN and temperature thresholds are never sent or read;
//! * autonomous power state transitions are switched off on every
//!   controller, so "no APST" and "not the deepest power state" hold for
//!   all of them;
//! * every buffer handed to a controller is a whole 4 KiB page, which is
//!   what the alignment quirks ask for;
//! * there is no suspend.

/// What a controller needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Quirks(
    /// The bits.
    pub u32,
);

impl Quirks {
    /// Nothing.
    pub const NONE: Quirks = Quirks(0);
    /// `CSTS.RDY` must not be read for 2.3 s after `CC.EN` changes.
    pub const DELAY_BEFORE_READY: Quirks = Quirks(1 << 0);
    /// Identify answers only CNS 0 and 1: the active namespace list is not
    /// asked for, and namespace 1 is taken.
    pub const IDENTIFY_CNS: Quirks = Quirks(1 << 1);
    /// One command at a time.
    pub const QUEUE_DEPTH_ONE: Quirks = Quirks(1 << 2);
    /// MSI is broken: the controller is driven with MSI-X or not at all.
    pub const BROKEN_MSI: Quirks = Quirks(1 << 3);
    /// The controller is not NVMe as specified (128-byte submission
    /// entries, tags shared between queues): not driven.
    pub const UNSUPPORTED: Quirks = Quirks(1 << 4);

    /// Whether every quirk in `other` is set here.
    #[must_use]
    pub const fn contains(self, other: Quirks) -> bool {
        self.0 & other.0 == other.0
    }

    /// The quirks in either.
    #[must_use]
    pub const fn union(self, other: Quirks) -> Quirks {
        Quirks(self.0 | other.0)
    }
}

/// How long [`Quirks::DELAY_BEFORE_READY`] waits, in nanoseconds.
pub const READY_DELAY_NANOS: u64 = 2_300_000_000;

/// The controllers known by PCI identity.
const BY_DEVICE: &[(u16, u16, Quirks)] = &[
    // QEMU's controller under its old Intel identity (`use-intel-id=on`).
    (0x8086, 0x5845, Quirks::IDENTIFY_CNS),
    // Seagate Nytro, HGST and WDC SN200 adapters, Memblaze Pblaze4,
    // Samsung PM1725 and PM1725a.
    (0x1BB1, 0x0100, Quirks::DELAY_BEFORE_READY),
    (0x1C58, 0x0003, Quirks::DELAY_BEFORE_READY),
    (0x1C58, 0x0023, Quirks::DELAY_BEFORE_READY),
    (0x1C5F, 0x0540, Quirks::DELAY_BEFORE_READY),
    (0x144D, 0xA821, Quirks::DELAY_BEFORE_READY),
    (0x144D, 0xA822, Quirks::DELAY_BEFORE_READY),
    // WD/SanDisk PC SN530 and SN550.
    (0x15B7, 0x5008, Quirks::BROKEN_MSI),
    (0x15B7, 0x5009, Quirks::BROKEN_MSI),
    // Apple's controllers: the 2015 MacBooks' takes one command at a time;
    // the T2's is not NVMe as specified.
    (0x106B, 0x2001, Quirks::QUEUE_DEPTH_ONE),
    (0x106B, 0x2005, Quirks::UNSUPPORTED),
];

/// A model known by the vendor id and the start of the model number
/// Identify reports, where the PCI identity is shared with drives that
/// need nothing.
const BY_MODEL: &[(u16, &[u8], Quirks)] = &[(
    0x144D,
    b"Samsung Portable SSD X5",
    Quirks::DELAY_BEFORE_READY,
)];

/// The quirks of PCI function `vendor:device`.
#[must_use]
pub fn for_device(vendor: u16, device: u16) -> Quirks {
    BY_DEVICE
        .iter()
        .filter(|(v, d, _)| *v == vendor && *d == device)
        .fold(Quirks::NONE, |all, (_, _, quirks)| all.union(*quirks))
}

/// The quirks of the controller whose Identify reports `vendor` and
/// `model`, beyond its PCI identity's.
#[must_use]
pub fn for_model(vendor: u16, model: &[u8]) -> Quirks {
    BY_MODEL
        .iter()
        .filter(|(v, name, _)| *v == vendor && model.starts_with(name))
        .fold(Quirks::NONE, |all, (_, _, quirks)| all.union(*quirks))
}
