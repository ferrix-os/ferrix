//! The RAM scrubber (`docs/RAM-SCRUBBER.md`): free frames claimed one at a
//! time, pattern-tested with writes that read nothing and an eviction to the
//! point of coherency, and a frame that fails its retest retired, while the
//! machine idles. The pace is ring 3's (`memscrub`); every bound is here.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_frame::Frame;

use super::{RETIRED_COUNT, Retired, holds_retired, keep_retired, retire_frame, with_frames};
use crate::sync::SpinLock;

/// The most frames one batch tests.
pub(crate) const BUDGET_MOST: u32 = 64;

/// The scrubber's share of the retired set: the rest is the machine
/// check's (`L.mm.105`).
pub(crate) const RETIRED_SHARE: usize = 192;

/// How many suspects -- frames that failed once and passed the retest --
/// are remembered, the oldest dropped first.
const SUSPECTS: usize = 64;

/// What the firmware says the memory does about errors, as SMBIOS type 16
/// reads (`ferrix_smbios::Correction`, mapped by the caller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Memory {
    /// Every system memory array says None.
    None,
    /// An array says Parity, ECC or CRC.
    Corrects,
    /// Other or Unknown, and nothing corrects.
    Unclear,
    /// No system memory array, or no table.
    Absent,
    /// A type 16 that cannot be read.
    Malformed,
}

/// Why the scrubber stands down, or that it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Standing {
    /// Not decided yet: nothing runs.
    Undecided = 0,
    /// It runs.
    Runs = 1,
    /// It runs because the gate overrode a stand-down.
    Overridden = 2,
    /// The memory corrects errors itself.
    Corrects = 3,
    /// The memory table cannot be read.
    Malformed = 4,
    /// Unused since the architecture is judged first: kept so the numbers
    /// stay as recorded.
    Unclear = 5,
    /// A hypervisor runs this kernel.
    Guest = 6,
    /// This architecture has no write of a whole line that reads nothing.
    NoWrite = 7,
}

impl Standing {
    /// The reason on the boot line and in `/proc/ferrix-scrub`.
    pub const fn reason(self) -> &'static str {
        match self {
            Standing::Undecided => "not decided",
            Standing::Runs => "runs",
            Standing::Overridden => "runs: stand-down overridden for the gate",
            Standing::Corrects => "stands down: the firmware says the memory corrects errors",
            Standing::Malformed => "stands down: the firmware's memory table cannot be read",
            Standing::Unclear => {
                "stands down: the firmware does not say whether the memory corrects errors"
            }
            Standing::Guest => "stands down: a hypervisor runs this kernel",
            Standing::NoWrite => "stands down: no write of a whole line here reads nothing",
        }
    }

    /// Whether it runs.
    pub const fn runs(self) -> bool {
        matches!(self, Standing::Runs | Standing::Overridden)
    }

    /// From its number.
    const fn from_number(number: u8) -> Standing {
        match number {
            1 => Standing::Runs,
            2 => Standing::Overridden,
            3 => Standing::Corrects,
            4 => Standing::Malformed,
            5 => Standing::Unclear,
            6 => Standing::Guest,
            7 => Standing::NoWrite,
            _ => Standing::Undecided,
        }
    }
}

/// The decision, made once at boot.
static STANDING: AtomicU8 = AtomicU8::new(Standing::Undecided as u8);

/// The boot option the gate passes to run the scrubber under QEMU, whose
/// type 16 says multi-bit ECC (`docs/RAM-SCRUBBER.md` §3.3).
#[cfg(ferrix_checks = "on")]
const TEST_OPTION: &str = "ferrix.scrub";

/// Whether the gate's override was asked for. It is compiled into every
/// build with the checks on, certified profiles included, so what keeps it
/// off bare metal is [`decide`]'s test: it is honoured only where a
/// hypervisor is positively seen (ledger 5057 (2), 5140 (1)); the option is
/// test-only, refused in a certified profile's command line (`AoU-38`).
#[cfg(ferrix_checks = "on")]
fn override_asked(view: &BootView<'_>) -> bool {
    view.option(TEST_OPTION) == Some("test")
}

/// A build without the checks has no override.
#[cfg(not(ferrix_checks = "on"))]
fn override_asked(_view: &BootView<'_>) -> bool {
    false
}

/// Whether the scrubber runs (`L.mm.106`): it stands down where the
/// firmware says the memory corrects errors or its table cannot be read,
/// where this processor has no write that reads nothing, and as a guest. The
/// gate's override is honoured only where a hypervisor is positively seen,
/// and only in a build with the checks. Decides nothing: [`decide`] keeps it,
/// and stage 2's check asks it first.
pub(crate) fn verdict(view: &BootView<'_>, memory: Memory) -> Standing {
    let ready = crate::arch::scrub_ready();
    // The architecture first: a processor with no write that reads nothing
    // (ARMv7-A, AArch64 with DZP) stands down whatever the table says.
    let mut standing = match memory {
        _ if ready.is_err() => Standing::NoWrite,
        Memory::Corrects => Standing::Corrects,
        Memory::Malformed => Standing::Malformed,
        Memory::None | Memory::Unclear | Memory::Absent => Standing::Runs,
    };
    if standing == Standing::Runs && crate::arch::is_guest(view) {
        standing = Standing::Guest;
    }
    if !standing.runs()
        && ready.is_ok()
        && crate::arch::hypervisor_seen(view)
        && override_asked(view)
    {
        standing = Standing::Overridden;
    }
    standing
}

/// Decide, once, whether the scrubber runs ([`verdict`]), and say so.
pub fn decide(view: &BootView<'_>, memory: Memory) -> Standing {
    let standing = verdict(view, memory);
    STANDING.store(standing as u8, Ordering::Release);
    crate::println!("scrub: {}", standing.reason());
    standing
}

// CHECKS-ONLY: syscall/native_check.rs; home: mm::scrub
/// The decision.
pub fn standing() -> Standing {
    Standing::from_number(STANDING.load(Ordering::Acquire))
}

/// Why a batch did not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The scrubber stands down.
    StandsDown,
    /// Another batch runs.
    Busy,
    /// A budget of 0 or past [`BUDGET_MOST`].
    Budget,
}

/// What one batch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    /// Frames tested.
    pub tested: u32,
    /// Frames that failed once and passed the retest.
    pub suspects: u32,
    /// Frames found bad.
    pub bad: u32,
    /// Bad frames retired.
    pub retired: u32,
    /// Bad frames held out because the retired set was full or the
    /// scrubber's share of it used.
    pub held: u32,
    /// Sweeps of the whole range this batch completed.
    pub sweeps: u32,
    /// Whether the batch stopped early: the watermark, or the retired set.
    pub stopped: bool,
}

/// One batch at a time, machine-wide (C1).
static BUSY: AtomicBool = AtomicBool::new(false);
/// The next frame to look at.
static CURSOR: AtomicU64 = AtomicU64::new(0);
/// The counts `/proc/ferrix-scrub` shows.
static TESTED: AtomicU64 = AtomicU64::new(0);
static SWEEP_TESTED: AtomicU64 = AtomicU64::new(0);
static SUSPECTED: AtomicU64 = AtomicU64::new(0);
static BAD: AtomicU64 = AtomicU64::new(0);
static RETIRED_BY_SCRUB: AtomicU64 = AtomicU64::new(0);
static HELD: AtomicU64 = AtomicU64::new(0);
static SWEEPS: AtomicU64 = AtomicU64::new(0);
/// Frames retired by the scrubber, so that its share is its own.
static SHARE_USED: AtomicU32 = AtomicU32::new(0);

/// The suspects, each frame plus one (0 empty), oldest overwritten.
static SUSPECT_LIST: SpinLock<([Frame; SUSPECTS], usize)> = SpinLock::new(([0; SUSPECTS], 0));

/// Test up to `budget` free frames from the cursor (`L.mm.101` to
/// `L.mm.105`): one at a time, each claimed under `FRAMES` only when the
/// claim leaves the allocator able to meet every request it could meet
/// before, tested, and released or kept.
///
/// # Errors
///
/// The [`Refusal`]; nothing tested.
pub fn test_batch(budget: u32) -> Result<Batch, Refusal> {
    if budget == 0 || budget > BUDGET_MOST {
        return Err(Refusal::Budget);
    }
    if !standing().runs() {
        return Err(Refusal::StandsDown);
    }
    if BUSY.swap(true, Ordering::AcqRel) {
        return Err(Refusal::Busy);
    }
    let batch = run(budget);
    BUSY.store(false, Ordering::Release);
    Ok(batch)
}

/// The batch, under [`BUSY`].
fn run(budget: u32) -> Batch {
    let mut batch = Batch::default();
    let Some((base, count)) = with_frames(|frames| frames.span()) else {
        return batch;
    };
    let mut looked = 0u64;
    while batch.tested < budget && looked < count {
        if share_used() {
            batch.stopped = true;
            break;
        }
        let at = CURSOR.load(Ordering::Relaxed) % count.max(1);
        let next = at + 1;
        if next >= count {
            CURSOR.store(0, Ordering::Relaxed);
            batch.sweeps += 1;
            let _ = SWEEPS.fetch_add(1, Ordering::Relaxed);
            SWEEP_TESTED.store(0, Ordering::Relaxed);
        } else {
            CURSOR.store(next, Ordering::Relaxed);
        }
        looked += 1;
        let frame = base + at;
        match claim(frame) {
            Claim::Taken => {}
            Claim::Skipped => continue,
            Claim::Low => {
                batch.stopped = true;
                break;
            }
        }
        batch.tested += 1;
        let _ = TESTED.fetch_add(1, Ordering::Relaxed);
        let _ = SWEEP_TESTED.fetch_add(1, Ordering::Relaxed);
        match judge(frame) {
            Verdict::Good => release(frame),
            Verdict::Suspect => {
                batch.suspects += 1;
                let _ = SUSPECTED.fetch_add(1, Ordering::Relaxed);
                release(frame);
            }
            Verdict::Bad => {
                batch.bad += 1;
                let _ = BAD.fetch_add(1, Ordering::Relaxed);
                if keep_bad(frame) {
                    batch.retired += 1;
                } else {
                    batch.held += 1;
                    batch.stopped = true;
                    break;
                }
            }
        }
    }
    batch
}

/// Whether the scrubber's share of the retired set is used, or the set is
/// full: it stops either way, so at most one frame is ever held out
/// (ledger 5057 (4)).
fn share_used() -> bool {
    SHARE_USED.load(Ordering::Relaxed) as usize >= RETIRED_SHARE
        || RETIRED_COUNT.load(Ordering::Relaxed) >= super::RETIRED_CAPACITY
}

/// What [`claim`] did.
enum Claim {
    /// The frame is the scrubber's.
    Taken,
    /// Not free, retired, or the only free block of its order or larger.
    Skipped,
    /// Free memory is below twice the watermark: the batch stops.
    Low,
}

/// Claim `frame`, in one hold of `FRAMES` (`L.mm.101`): only if it is free,
/// not retired, free memory is at least twice the watermark, and another
/// free block of its block's order or larger exists. Counts nothing: the
/// release is uncounted too (C4).
fn claim(frame: Frame) -> Claim {
    let watermark = super::watermark();
    with_frames(|frames| {
        if frames.free_frames() < watermark.saturating_mul(2) {
            return Claim::Low;
        }
        let Some((head, order)) = frames.free_block_of(frame) else {
            return Claim::Skipped;
        };
        if holds_retired(frame, 0) || !frames.another_free_block(head, order) {
            return Claim::Skipped;
        }
        if frames.claim(frame).is_some() {
            Claim::Taken
        } else {
            Claim::Skipped
        }
    })
    .unwrap_or(Claim::Skipped)
}

/// Release `frame`, zeroed, through the free path that makes
/// `deallocate_frames`'s retired-frame decision, uncounted.
fn release(frame: Frame) {
    zero(frame);
    let _ = with_frames(|frames| {
        if holds_retired(frame, 0) {
            let _ = keep_retired(frames, frame, 0);
        } else {
            let _ = frames.deallocate(frame, 0);
        }
    });
}

/// Keep a bad frame (`L.mm.105`): retired, then freed through
/// [`release`]'s decision, so it stays allocated, owned by nobody, in the
/// retired set; or, where the set is full, held, claimed, out of use.
/// Whether it was retired.
fn keep_bad(frame: Frame) -> bool {
    let retired = retire_frame(frame);
    match retired {
        Retired::Marked | Retired::Claimed | Retired::Already => {
            // A frame the machine check retired first is not the
            // scrubber's to count against its share.
            if !matches!(retired, Retired::Already) {
                let _ = SHARE_USED.fetch_add(1, Ordering::Relaxed);
            }
            let _ = RETIRED_BY_SCRUB.fetch_add(1, Ordering::Relaxed);
            release(frame);
            crate::println!("scrub: a frame failed its pattern test twice and was retired");
            true
        }
        Retired::Full | Retired::NotCovered => {
            let _ = HELD.fetch_add(1, Ordering::Relaxed);
            crate::println!(
                "scrub: a frame failed its pattern test twice and is held: the retired set is full"
            );
            false
        }
    }
}

/// A frame's test result.
enum Verdict {
    Good,
    Suspect,
    Bad,
}

/// Test `frame`, and again at once if it fails (`L.mm.104`): bad if the
/// retest fails too or it failed before as a suspect; a suspect if only the
/// first test failed.
fn judge(frame: Frame) -> Verdict {
    if test(frame) {
        return Verdict::Good;
    }
    if !test(frame) || was_suspect(frame) {
        return Verdict::Bad;
    }
    remember_suspect(frame);
    Verdict::Suspect
}

/// Whether `frame` failed before, on an earlier sweep.
fn was_suspect(frame: Frame) -> bool {
    SUSPECT_LIST
        .lock()
        .0
        .iter()
        .any(|&stored| stored == frame + 1)
}

/// Remember `frame` as a suspect, over the oldest.
fn remember_suspect(frame: Frame) {
    let mut list = SUSPECT_LIST.lock();
    let at = list.1 % SUSPECTS;
    if let Some(slot) = list.0.get_mut(at) {
        *slot = frame + 1;
    }
    list.1 = at + 1;
}

/// The four passes over one frame (`L.mm.103`): `0x5555..`, `0xAAAA..`, each
/// word its own physical address, and that inverted; each written with
/// writes that read nothing, evicted, read back and compared. Whether every
/// word read back as written.
fn test(frame: Frame) -> bool {
    let phys = frame * PAGE_SIZE;
    let virt = super::direct_map(phys);
    let passes: [&dyn Fn(u64) -> u64; 4] = [
        &|_| 0x5555_5555_5555_5555,
        &|_| 0xAAAA_AAAA_AAAA_AAAA,
        &|offset| phys + offset,
        &|offset| !(phys + offset),
    ];
    for value in passes {
        crate::arch::fill_without_reading(virt, PAGE_SIZE, value);
        crate::arch::evict_to_coherency(virt, PAGE_SIZE);
        let mut offset = 0;
        while offset < PAGE_SIZE {
            // SAFETY: (KMEM) an aligned word of the frame the scrubber claimed,
            // through the direct map.
            let read = unsafe { core::ptr::read_volatile((virt + offset) as *const u64) };
            if injected(frame, read) != value(offset) {
                return false;
            }
            offset += 8;
        }
    }
    true
}

/// Zero `frame` before it goes back, with writes that read nothing.
fn zero(frame: Frame) {
    let virt = super::direct_map(frame * PAGE_SIZE);
    crate::arch::fill_without_reading(virt, PAGE_SIZE, &|_| 0);
}

/// The stage check's injected fault: a frame plus one, and how many of its
/// reads still to flip (0 none); a flipped read has bit 0 inverted. Check
/// code alone sets it ([`inject`]).
static INJECT_FRAME: AtomicU64 = AtomicU64::new(0);
/// How many tests of [`INJECT_FRAME`] still fail.
static INJECT_TESTS: AtomicU32 = AtomicU32::new(0);

/// A read of `frame` as the injection leaves it: bit 0 flipped while the
/// injected frame has tests left to fail. A test counts once, at its first
/// word.
fn injected(frame: Frame, read: u64) -> u64 {
    if INJECT_FRAME.load(Ordering::Relaxed) != frame + 1 {
        return read;
    }
    let left = INJECT_TESTS.load(Ordering::Relaxed);
    if left == 0 {
        return read;
    }
    INJECT_TESTS.store(left - 1, Ordering::Relaxed);
    read ^ 1
}

// CHECKS-ONLY: mm/check.rs; home: mm::scrub
/// Make the next `tests` tests of `frame` fail, for the stage check: one
/// makes a suspect, two a bad frame.
pub fn inject(frame: Frame, tests: u32) {
    INJECT_TESTS.store(tests, Ordering::Relaxed);
    INJECT_FRAME.store(frame + 1, Ordering::Relaxed);
}

// CHECKS-ONLY: mm/check.rs; home: mm::scrub
/// A batch for the stage check, whatever the boot decided: the check runs
/// at stage 2, before the decision, on any machine whose processor can
/// write without reading.
pub(crate) fn batch_for_check(budget: u32) -> Batch {
    if BUSY.swap(true, Ordering::AcqRel) {
        return Batch::default();
    }
    let batch = run(budget);
    BUSY.store(false, Ordering::Release);
    batch
}

// CHECKS-ONLY: mm/check.rs; home: mm::scrub
/// Forget what the stage check counted: the counts, the share and the
/// suspects, so the boot starts from nothing.
pub(crate) fn reset_for_check() {
    for count in [
        &TESTED,
        &SWEEP_TESTED,
        &SUSPECTED,
        &BAD,
        &RETIRED_BY_SCRUB,
        &HELD,
        &SWEEPS,
    ] {
        count.store(0, Ordering::Relaxed);
    }
    SHARE_USED.store(0, Ordering::Relaxed);
    *SUSPECT_LIST.lock() = ([0; SUSPECTS], 0);
    INJECT_TESTS.store(0, Ordering::Relaxed);
    INJECT_FRAME.store(0, Ordering::Relaxed);
    CURSOR.store(0, Ordering::Relaxed);
}

// CHECKS-ONLY: mm/check.rs; home: mm::scrub
/// Point the cursor at `frame`, for the stage check.
pub(crate) fn aim(frame: Frame) {
    let base = with_frames(|frames| frames.span().0).unwrap_or(0);
    CURSOR.store(frame.saturating_sub(base), Ordering::Relaxed);
}

/// `/proc/ferrix-scrub`'s text (`L.mm.107`): frames managed and tested, the
/// judgements, the sweeps, and the standing. No addresses.
pub fn render(out: &mut impl core::fmt::Write) -> core::fmt::Result {
    writeln!(out, "standing {}", standing().reason())?;
    writeln!(out, "managed {}", super::managed_frames())?;
    writeln!(out, "tested {}", TESTED.load(Ordering::Relaxed))?;
    writeln!(
        out,
        "tested-this-sweep {}",
        SWEEP_TESTED.load(Ordering::Relaxed)
    )?;
    writeln!(out, "sweeps {}", SWEEPS.load(Ordering::Relaxed))?;
    writeln!(out, "suspects {}", SUSPECTED.load(Ordering::Relaxed))?;
    writeln!(out, "bad {}", BAD.load(Ordering::Relaxed))?;
    writeln!(out, "retired {}", RETIRED_BY_SCRUB.load(Ordering::Relaxed))?;
    writeln!(out, "held {}", HELD.load(Ordering::Relaxed))
}
