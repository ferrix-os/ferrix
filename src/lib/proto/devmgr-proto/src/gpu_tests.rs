//! `devmgr`'s hand-over of a GPU to `nvrm` (`docs/NVIDIA.md` §12.2, §12.3;
//! N1b): the mark, then isolation, then the budget, and a named line for
//! every outcome.

extern crate std;

use std::cell::RefCell;
use std::format;
use std::vec::Vec;

use crate::budget::{GIB_PAGES, GpuBudget, PAGE_BYTES};
use crate::gpu::{self, HandOver, Node, Place, SetRefused, Unanswered};

/// Pages in `mib` MiB.
const fn mib(mib: u64) -> u64 {
    mib * 1024 * 1024 / PAGE_BYTES
}

/// The ceiling on a machine of `ram_gib` GiB: a quarter of it.
const fn ceiling(ram_gib: u64) -> u64 {
    ram_gib * GIB_PAGES / 4
}

/// The PCI address word of 01:00.0, where libvirt puts the 3060.
const CARD: u32 = 0x0000_0100;

/// A device as the kernel would answer for it, recording what was asked.
struct Fake {
    mark: Result<(), Unanswered>,
    isolation: Result<u64, Unanswered>,
    ceiling: Result<u64, Unanswered>,
    room: Result<u64, Unanswered>,
    set: Result<(), SetRefused>,
    asked: RefCell<Vec<&'static str>>,
    budget: RefCell<Option<u64>>,
}

impl Fake {
    /// An isolated GPU on a machine of `ram_gib` GiB with nothing else
    /// raised.
    fn isolated(ram_gib: u64) -> Fake {
        Fake {
            mark: Ok(()),
            isolation: Ok(gpu::INTERRUPTS_ISOLATED | gpu::DMA_TRANSLATED),
            ceiling: Ok(ceiling(ram_gib)),
            room: Ok(ceiling(ram_gib)),
            set: Ok(()),
            asked: RefCell::new(Vec::new()),
            budget: RefCell::new(None),
        }
    }

    fn asked(&self) -> Vec<&'static str> {
        self.asked.borrow().clone()
    }
}

impl Node for Fake {
    fn mark(&self) -> Result<(), Unanswered> {
        self.asked.borrow_mut().push("mark");
        self.mark
    }
    fn isolation(&self) -> Result<u64, Unanswered> {
        self.asked.borrow_mut().push("isolation");
        self.isolation
    }
    fn ceiling(&self) -> Result<u64, Unanswered> {
        self.asked.borrow_mut().push("ceiling");
        self.ceiling
    }
    fn room(&self) -> Result<u64, Unanswered> {
        self.asked.borrow_mut().push("room");
        self.room
    }
    fn set_budget(&self, pages: u64) -> Result<(), SetRefused> {
        self.asked.borrow_mut().push("set");
        if self.set.is_ok() {
            *self.budget.borrow_mut() = Some(pages);
        }
        self.set
    }
}

#[test]
fn the_bit_is_the_abis() {
    assert_eq!(
        gpu::INTERRUPTS_ISOLATED,
        ferrix_native_abi::types::DEVICE_ISOLATION_INTERRUPTS
    );
    assert_eq!(
        gpu::DMA_TRANSLATED,
        ferrix_native_abi::types::DEVICE_ISOLATION_DMA_TRANSLATED
    );
}

#[test]
fn an_isolated_gpu_is_marked_then_budgeted_then_started() {
    let node = Fake::isolated(16);
    let outcome = gpu::hand_over(&node);
    assert_eq!(
        outcome,
        HandOver::Start {
            isolation: 3,
            budget: GpuBudget::Whole { pages: mib(2048) },
        }
    );
    assert!(outcome.starts());
    assert_eq!(
        node.asked(),
        ["mark", "isolation", "ceiling", "room", "set"],
        "the mark first, the budget set last, before the driver starts"
    );
    assert_eq!(*node.budget.borrow(), Some(mib(2048)));
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0: marked for isolated interrupts, device_isolation 0x3 \
         (interrupts isolated); handing it to nvrm"
    );
}

#[test]
fn a_cut_budget_still_starts() {
    let node = Fake::isolated(4);
    let outcome = gpu::hand_over(&node);
    let HandOver::Start { budget, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        format!("{}", budget.line(CARD)),
        "devmgr   gpu 01:00.0: pin budget 512 MiB, cut from 1024 MiB: the 1 GiB floor \
         yields to the kernel's ceiling (4096 MiB of RAM)"
    );
    assert_eq!(*node.budget.borrow(), Some(mib(512)));
}

#[test]
fn a_refused_mark_stops_before_anything_else() {
    let node = Fake {
        mark: Err(Unanswered),
        ..Fake::isolated(16)
    };
    let outcome = gpu::hand_over(&node);
    assert_eq!(outcome, HandOver::MarkRefused);
    assert!(!outcome.starts());
    assert_eq!(node.asked(), ["mark"]);
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0 not started: the kernel refused its isolated-interrupts mark"
    );
}

#[test]
fn unisolated_interrupts_stop_the_hand_over_after_the_mark() {
    let node = Fake {
        isolation: Ok(gpu::DMA_TRANSLATED),
        ..Fake::isolated(16)
    };
    let outcome = gpu::hand_over(&node);
    assert_eq!(outcome, HandOver::Unisolated { isolation: 1 });
    assert!(!outcome.starts());
    assert_eq!(
        node.asked(),
        ["mark", "isolation"],
        "marked, so the kernel's guard holds too; no budget raised for a GPU never started"
    );
    assert_eq!(*node.budget.borrow(), None);
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0 not started: its interrupts are not isolated (device_isolation \
         0x1)"
    );
}

/// A unit held to interrupt remapping only -- by an RMRR, an IVMD, or a
/// unit that does not translate -- answers bit 1 without bit 0: the GPU
/// is not handed over, though its interrupts are isolated (`L.device.56`).
///
/// Verifies: L.device.56
#[test]
fn a_gpu_behind_a_remapping_only_unit_is_not_handed_over() {
    let node = Fake {
        isolation: Ok(gpu::INTERRUPTS_ISOLATED),
        ..Fake::isolated(16)
    };
    let outcome = gpu::hand_over(&node);
    assert_eq!(outcome, HandOver::Untranslated { isolation: 2 });
    assert!(!outcome.starts());
    assert_eq!(
        node.asked(),
        ["mark", "isolation"],
        "marked first, as for unisolated interrupts; no budget raised for a GPU never started"
    );
    assert_eq!(*node.budget.borrow(), None);
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0 not started: its DMA is not translated (device_isolation 0x2)"
    );
}

#[test]
fn an_unreadable_isolation_stops_the_hand_over() {
    let node = Fake {
        isolation: Err(Unanswered),
        ..Fake::isolated(16)
    };
    let outcome = gpu::hand_over(&node);
    assert_eq!(outcome, HandOver::IsolationUnread);
    assert_eq!(node.asked(), ["mark", "isolation"]);
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0 not started: its device_isolation could not be read"
    );
}

#[test]
fn a_budget_too_small_is_never_set() {
    let node = Fake::isolated(1);
    let outcome = gpu::hand_over(&node);
    assert_eq!(
        outcome,
        HandOver::Budget(GpuBudget::TooSmall { pages: mib(128) })
    );
    assert_eq!(node.asked(), ["mark", "isolation", "ceiling", "room"]);
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0 not started: its pin budget of 128 MiB is under the 256 MiB \
         nvrm needs"
    );
}

#[test]
fn a_budget_past_the_ceiling_is_refused_with_no_fallback() {
    let node = Fake {
        set: Err(SetRefused::PastCeiling),
        ..Fake::isolated(4)
    };
    let outcome = gpu::hand_over(&node);
    assert_eq!(
        outcome,
        HandOver::Budget(GpuBudget::Refused { pages: mib(512) })
    );
    assert!(!outcome.starts());
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0 not started: the kernel refused its pin budget of 512 MiB \
         (past the ceiling)"
    );
}

#[test]
fn any_other_refusal_of_the_budget_stops_the_hand_over() {
    let node = Fake {
        set: Err(SetRefused::Other),
        ..Fake::isolated(8)
    };
    let outcome = gpu::hand_over(&node);
    assert_eq!(outcome, HandOver::BudgetFailed { pages: GIB_PAGES });
    assert!(!outcome.starts());
    assert_eq!(
        format!("{}", outcome.line(CARD)),
        "devmgr   gpu 01:00.0 not started: the kernel refused its pin budget of 1024 MiB"
    );
}

#[test]
fn unreadable_limits_stop_the_hand_over() {
    let node = Fake {
        room: Err(Unanswered),
        ..Fake::isolated(8)
    };
    assert_eq!(gpu::hand_over(&node), HandOver::LimitsUnread);
    assert_eq!(*node.budget.borrow(), None);
    assert_eq!(
        format!("{}", HandOver::LimitsUnread.line(CARD)),
        "devmgr   gpu 01:00.0 not started: its pin ceiling and room could not be read"
    );
}

#[test]
fn places_are_written_as_the_budget_writes_them() {
    assert_eq!(format!("{}", Place(CARD)), "01:00.0");
    assert_eq!(format!("{}", Place(0x0000_0010)), "00:02.0");
    assert_eq!(format!("{}", Place(0x0001_0100)), "0001:01:00.0");
    let refused = HandOver::MarkRefused.line(0x0001_0100);
    assert_eq!(
        format!("{refused}"),
        "devmgr   gpu 0001:01:00.0 not started: the kernel refused its isolated-interrupts mark"
    );
}
