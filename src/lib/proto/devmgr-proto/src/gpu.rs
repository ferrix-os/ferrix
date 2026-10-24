//! The hand-over of a GPU to `nvrm`, as `devmgr`'s `Gpu` kind makes it
//! (`docs/NVIDIA.md` §4.1, §12.2 and §12.3; NVIDIA's N1b).
//!
//! A GPU runs firmware of its own -- NVIDIA's GSP -- which is not the
//! driver's to vouch for, and its driver pins far more than any other. So
//! before `nvrm` is started on one, three things must hold, in this order:
//!
//! 1. **The mark** ([`crate::isolation`]): the device's isolated-interrupts
//!    mark is set, set-once, so the kernel refuses the device vectors and
//!    pins whenever the machine's interrupts are not isolated. A mark the
//!    kernel refuses stops the launch. It comes first so that the kernel's
//!    own guard is armed whatever `devmgr` decides next.
//! 2. **Isolation**: `device_isolation` says the machine's interrupts are
//!    isolated and this device's messages are remapped (bit 1), and that
//!    an IOMMU translates its DMA (bit 0). Without bit 1 the GPU is not
//!    handed over at all: the kernel would refuse `nvrm` its vectors and
//!    pins anyway, and a driver started only to be refused is a launch that
//!    hides why. This is where F-57's closure is checked for the
//!    `ferrix-3060` domain at N1's first boot. Without bit 0 it is not
//!    handed over either (`L.device.56`): a GPU executes what programs hand
//!    it, so with its DMA untranslated -- a unit held to interrupt
//!    remapping only, by an RMRR, an IVMD, or no translation -- one bad
//!    page table entry is DMA anywhere, though its interrupts are isolated.
//!    The kernel answers bit 0 for the domain the device gets before any
//!    pin (`L.device.55`), so the answer is the driver's.
//! 3. **The budget** ([`crate::budget`]): the pin budget is planned from
//!    the kernel's ceiling and room and set with `device_set_limit`, before
//!    the driver starts, since the kernel refuses a change under live pins.
//!    A budget too small, or one the kernel refuses, stops the launch; there
//!    is no fallback to the default.
//!
//! Every outcome is one named line. A pure function of what the kernel
//! answers, tested on the host; `devmgr` supplies the calls.

use core::fmt;

use crate::budget::GpuBudget;
use crate::isolation::{self, Launch, Refusal};

/// `device_isolation`'s bit for interrupts isolated:
/// `ferrix_native_abi::types::DEVICE_ISOLATION_INTERRUPTS`, which a host test
/// holds this to.
pub const INTERRUPTS_ISOLATED: u64 = 1 << 1;

/// `device_isolation`'s bit for DMA translated:
/// `ferrix_native_abi::types::DEVICE_ISOLATION_DMA_TRANSLATED`.
pub const DMA_TRANSLATED: u64 = 1 << 0;

/// A call the kernel did not answer with what was asked: any refusal, which
/// the hand-over treats alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unanswered;

/// Why `device_set_limit` refused a budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetRefused {
    /// `NO_MEMORY`: past the kernel's ceiling, another device having taken
    /// room since it was read.
    PastCeiling,
    /// Anything else: `BAD_STATE` under live pins, `ACCESS_DENIED` without
    /// `SET_LIMIT`.
    Other,
}

/// The calls the hand-over makes on the device, through the handle `devmgr`
/// keeps with `SET_LIMIT`.
pub trait Node {
    /// `device_set_limit(DEVICE_LIMIT_ISOLATED_INTERRUPTS, 1)`.
    ///
    /// # Errors
    ///
    /// [`Unanswered`] for the kernel's refusal.
    fn mark(&self) -> Result<(), Unanswered>;
    /// `device_isolation`.
    ///
    /// # Errors
    ///
    /// [`Unanswered`] for the kernel's refusal.
    fn isolation(&self) -> Result<u64, Unanswered>;
    /// `device_get_limit(DEVICE_LIMIT_PIN_CEILING)`, in pages.
    ///
    /// # Errors
    ///
    /// [`Unanswered`] for the kernel's refusal.
    fn ceiling(&self) -> Result<u64, Unanswered>;
    /// `device_get_limit(DEVICE_LIMIT_PIN_ROOM)`, in pages.
    ///
    /// # Errors
    ///
    /// [`Unanswered`] for the kernel's refusal.
    fn room(&self) -> Result<u64, Unanswered>;
    /// `device_set_limit(DEVICE_LIMIT_PIN_PAGES, pages)`.
    ///
    /// # Errors
    ///
    /// [`SetRefused`].
    fn set_budget(&self, pages: u64) -> Result<(), SetRefused>;
}

/// What the hand-over decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandOver {
    /// Start `nvrm`: the device is marked, its interrupts are isolated, and
    /// its budget -- [`GpuBudget::Whole`] or [`GpuBudget::Cut`] -- is set.
    Start {
        /// What `device_isolation` answered.
        isolation: u64,
        /// The budget set.
        budget: GpuBudget,
    },
    /// Not started: the kernel refused the isolated-interrupts mark.
    MarkRefused,
    /// Not started: `device_isolation` could not be read.
    IsolationUnread,
    /// Not started: the machine's interrupts are not isolated, or this
    /// device's are not remapped.
    Unisolated {
        /// What `device_isolation` answered.
        isolation: u64,
    },
    /// Not started: its interrupts are isolated, but no IOMMU translates
    /// its DMA (`L.device.56`).
    Untranslated {
        /// What `device_isolation` answered.
        isolation: u64,
    },
    /// Not started: the ceiling or the room could not be read.
    LimitsUnread,
    /// Not started: the budget is [`GpuBudget::TooSmall`] or
    /// [`GpuBudget::Refused`].
    Budget(GpuBudget),
    /// Not started: the kernel refused the budget other than as past its
    /// ceiling.
    BudgetFailed {
        /// The budget refused, in pages.
        pages: u64,
    },
}

/// Hand the GPU `node` over to `nvrm`, or say why not: the mark, then
/// isolation, then the budget, each only once the one before held.
pub fn hand_over(node: &impl Node) -> HandOver {
    if isolation::launch(true, || node.mark()) == Launch::Refused {
        return HandOver::MarkRefused;
    }
    let Ok(isolation) = node.isolation() else {
        return HandOver::IsolationUnread;
    };
    if isolation & INTERRUPTS_ISOLATED == 0 {
        return HandOver::Unisolated { isolation };
    }
    if isolation & DMA_TRANSLATED == 0 {
        return HandOver::Untranslated { isolation };
    }
    let (Ok(ceiling), Ok(room)) = (node.ceiling(), node.room()) else {
        return HandOver::LimitsUnread;
    };
    let plan = GpuBudget::plan(ceiling, room);
    let Some(pages) = plan.to_set() else {
        return HandOver::Budget(plan);
    };
    match node.set_budget(pages) {
        Ok(()) => HandOver::Start {
            isolation,
            budget: plan,
        },
        Err(SetRefused::PastCeiling) => HandOver::Budget(plan.refused()),
        Err(SetRefused::Other) => HandOver::BudgetFailed { pages },
    }
}

impl HandOver {
    /// The line `devmgr` says it with, for the GPU at `location`, the PCI
    /// address word. A start says it with this line and then its budget's
    /// ([`GpuBudget::line`]); every other outcome is this line alone.
    #[must_use]
    pub const fn line(self, location: u32) -> Line {
        Line {
            outcome: self,
            location,
        }
    }

    /// Whether `nvrm` is started.
    #[must_use]
    pub const fn starts(self) -> bool {
        matches!(self, HandOver::Start { .. })
    }
}

/// [`HandOver::line`]'s text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    /// The outcome.
    outcome: HandOver,
    /// The PCI address word.
    location: u32,
}

/// A PCI address word as `bb:dd.f`, with the segment first when it is not
/// zero, as [`GpuBudget::line`] writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place(pub u32);

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let segment = self.0 >> 16;
        let [devfn, bus, _, _] = self.0.to_le_bytes();
        if segment != 0 {
            write!(f, "{segment:04x}:")?;
        }
        write!(f, "{bus:02x}:{:02x}.{}", devfn >> 3, devfn & 7)
    }
}

/// A PCI address as [`Place`] writes it, in a buffer, for [`Refusal`]'s
/// `&str`.
struct Placed {
    bytes: [u8; 16],
    len: usize,
}

impl Placed {
    fn of(location: u32) -> Placed {
        let mut placed = Placed {
            bytes: [0; 16],
            len: 0,
        };
        let _ = fmt::write(&mut placed, format_args!("{}", Place(location)));
        placed
    }

    fn as_str(&self) -> &str {
        self.bytes
            .get(..self.len)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("?")
    }
}

impl fmt::Write for Placed {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        let slot = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = Place(self.location);
        let mib = |pages: u64| pages * crate::budget::PAGE_BYTES / (1024 * 1024);
        match self.outcome {
            HandOver::Start { isolation, .. } => write!(
                f,
                "devmgr   gpu {place}: marked for isolated interrupts, device_isolation \
                 {isolation:#x} (interrupts isolated); handing it to nvrm"
            ),
            HandOver::MarkRefused => {
                let placed = Placed::of(self.location);
                write!(
                    f,
                    "{}",
                    Refusal {
                        what: "gpu",
                        location: placed.as_str(),
                    }
                )
            }
            HandOver::IsolationUnread => write!(
                f,
                "devmgr   gpu {place} not started: its device_isolation could not be read"
            ),
            HandOver::Unisolated { isolation } => write!(
                f,
                "devmgr   gpu {place} not started: its interrupts are not isolated \
                 (device_isolation {isolation:#x})"
            ),
            HandOver::Untranslated { isolation } => write!(
                f,
                "devmgr   gpu {place} not started: its DMA is not translated \
                 (device_isolation {isolation:#x})"
            ),
            HandOver::LimitsUnread => write!(
                f,
                "devmgr   gpu {place} not started: its pin ceiling and room could not be read"
            ),
            HandOver::Budget(budget) => write!(f, "{}", budget.line(self.location)),
            HandOver::BudgetFailed { pages } => write!(
                f,
                "devmgr   gpu {place} not started: the kernel refused its pin budget of {} MiB",
                mib(pages)
            ),
        }
    }
}
