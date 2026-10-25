//! Whether `devmgr` may hand a device to a driver: every bus master only
//! once its DMA is translated and its interrupts isolated
//! (`docs/DMA-GATE.md`; the customer's decision, ledger 3905; consultant
//! OK IF (1)-(9), ledger 3906).
//!
//! The rule is [`admit`], a pure function of what `device_isolation`
//! answered and of whether the boot carries the opt-out word,
//! `ferrix.unisolated`, which the kernel forwards in DEVICES' flags. Its
//! answer to a device `devmgr` may start is an [`Admitted`], which only
//! [`admit`] makes and which `devmgr`'s one launch of a driver with a device
//! handle takes, so no path can hand a device over without it.
//!
//! * A node that reaches no memory -- `device_isolation`'s
//!   [`NO_DMA`] -- is admitted: there is no DMA to isolate.
//! * Otherwise the device needs [`DMA_TRANSLATED`] and, but on AArch64,
//!   [`INTERRUPTS_ISOLATED`]. On AArch64 the reference machine's `GICv2m`
//!   never isolates interrupts, and the customer chose the partial rule
//!   there (2026-10-24, "(c) a partial rule on 64-bit Arm"): translation
//!   alone, until GICv3 ITS rows exist, the interrupt residual as
//!   VULNERABILITY-ANALYSIS V-03 states it.
//! * A device short of what it needs is refused -- or, under the opt-out,
//!   started and said, loudly, every time.
//! * An unreadable `device_isolation` refuses the device, opt-out or not.

use core::fmt;

/// `device_isolation`'s bit 0: an IOMMU translates the device's DMA.
pub const DMA_TRANSLATED: u64 = 1 << 0;
/// `device_isolation`'s bit 1: the machine's interrupts are isolated and the
/// device's own messages remapped.
pub const INTERRUPTS_ISOLATED: u64 = 1 << 1;
/// `device_isolation`'s bit 2: the node reaches no memory.
pub const NO_DMA: u64 = 1 << 2;

/// What the rule requires of a bus master on this architecture: both bits,
/// but translation alone on AArch64 (the customer's G4, ledger 3906).
#[must_use]
pub const fn required(aarch64: bool) -> u64 {
    if aarch64 {
        DMA_TRANSLATED
    } else {
        DMA_TRANSLATED | INTERRUPTS_ISOLATED
    }
}

/// A device `devmgr` may hand to its driver: made by [`admit`] alone, for
/// the device at one location, and neither `Copy` nor `Clone`, so one
/// admission starts one device, the one it was made for (ledger 4153 (1)).
#[derive(Debug, PartialEq, Eq)]
pub struct Admitted {
    /// Whether only the opt-out admitted it.
    unisolated: bool,
    /// The PCI address word of the device it admits.
    location: u32,
}

impl Admitted {
    /// Whether the device is admitted only under `ferrix.unisolated`.
    #[must_use]
    pub const fn unisolated(&self) -> bool {
        self.unisolated
    }

    /// Whether this admission is the one for the device at `location`:
    /// `devmgr`'s `start` refuses a device it is not.
    #[must_use]
    pub const fn admits(&self, location: u32) -> bool {
        self.location == location
    }

    /// [`Admitted::admits`], taking the admission: `devmgr`'s `start` uses
    /// it up, so it starts one device.
    #[must_use]
    pub const fn starts(self, location: u32) -> bool {
        self.admits(location)
    }
}

/// What [`admit`] decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Admit {
    /// Hand the device over.
    Start(Admitted),
    /// Do not: `device_isolation` answered `bits`, short of `required`.
    Refused {
        /// What `device_isolation` answered.
        bits: u64,
        /// What the device needed.
        required: u64,
    },
    /// Do not: `device_isolation` could not be read.
    Unread,
}

/// Whether to hand over the device at `location` whose `device_isolation`
/// answered `bits` (`None`: unreadable), on AArch64 when `aarch64`, with the
/// opt-out word forwarded when `unisolated_allowed`.
#[must_use]
pub const fn admit(
    bits: Option<u64>,
    aarch64: bool,
    unisolated_allowed: bool,
    location: u32,
) -> Admit {
    let Some(bits) = bits else {
        return Admit::Unread;
    };
    let isolated = Admit::Start(Admitted {
        unisolated: false,
        location,
    });
    if bits & NO_DMA != 0 {
        return isolated;
    }
    let required = required(aarch64);
    if bits & required == required {
        return isolated;
    }
    if unisolated_allowed {
        return Admit::Start(Admitted {
            unisolated: true,
            location,
        });
    }
    Admit::Refused { bits, required }
}

/// What the kernel makes of `ferrix.unisolated` on a command line that
/// carries it as a bare word (`bare`) and with a value (`valued`): the
/// opt-out only for the bare word alone. A value, with or without the bare
/// word beside it, is said and gives no opt-out (ledger 3906 (2), 4153 (2)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptOut {
    /// Not on the command line.
    Absent,
    /// The bare word alone: devmgr may start unisolated devices.
    On,
    /// A value was given: said, and no opt-out.
    Ignored,
}

/// [`OptOut`] for a command line.
#[must_use]
pub const fn opt_out(bare: bool, valued: bool) -> OptOut {
    match (bare, valued) {
        (_, true) => OptOut::Ignored,
        (true, false) => OptOut::On,
        (false, false) => OptOut::Absent,
    }
}

impl Admit {
    /// The line `devmgr` says for `what` at the PCI address word `location`
    /// -- `None` for a device admitted by its isolation, which needs none.
    #[must_use]
    pub const fn line<'a>(&'a self, what: &'a str, location: u32) -> Option<Line<'a>> {
        match self {
            Admit::Start(Admitted {
                unisolated: false, ..
            }) => None,
            _ => Some(Line {
                admit: self,
                what,
                location,
            }),
        }
    }
}

/// [`Admit::line`]'s text.
#[derive(Debug, Clone, Copy)]
pub struct Line<'a> {
    /// The decision.
    admit: &'a Admit,
    /// The kind, as devmgr's lines name it.
    what: &'a str,
    /// Where the device is: its PCI address word, as [`crate::gpu::Place`]
    /// writes it.
    location: u32,
}

impl fmt::Display for Line<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Line {
            admit,
            what,
            location,
        } = *self;
        let location = crate::gpu::Place(location);
        match admit {
            Admit::Start(_) => write!(
                f,
                "devmgr   UNISOLATED: {what} {location} started under ferrix.unisolated: its \
                 DMA is not translated or its interrupts are not isolated, and its driver is \
                 trusted with all of memory"
            ),
            &Admit::Refused { bits, required } => {
                let missing = required & !bits;
                let why = if missing & DMA_TRANSLATED != 0 {
                    "its DMA is not translated"
                } else {
                    "its interrupts are not isolated"
                };
                write!(
                    f,
                    "devmgr   {what} {location} not started: {why} (device_isolation {bits:#x})"
                )
            }
            Admit::Unread => write!(
                f,
                "devmgr   {what} {location} not started: its device_isolation could not be read"
            ),
        }
    }
}
