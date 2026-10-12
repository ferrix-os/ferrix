//! Two cores changing neighbouring lines of the distributor at once lose
//! neither change (F-50).
//!
//! A priority or target register holds four lines' bytes and a configuration
//! register sixteen lines' bits, so each change to one line is a read, a
//! change and a write of a word other lines share. [`super::rmw`] holds a lock
//! across the three. This check is what shows that it does: two cores make an
//! SPI edge-triggered and enable it -- `msi_allocate`'s order, configuration
//! before enable as the architecture asks -- on neighbouring SPIs at the same
//! moment, and every line must come out with its priority, target and
//! configuration set. The race is not left to chance: while the check runs,
//! [`super::WIDEN_SPINS`] holds each core between its read and its write, so
//! that without the lock both read before either writes and one core's write
//! puts back what the other had changed, every round.
//!
//! The lines are ones nothing can own: the highest sixteen SPIs, one
//! configuration word, that no device node names, no handler is registered
//! on, the `GICv2m` frame does not hand out, and nothing has enabled. After
//! every round their priority, target, configuration and pending state go back
//! to what they were, read back, and they are left disabled.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::{
    DEFAULT_PRIORITY, DISTRIBUTOR, FIRST_SPECIAL_ID, GICD_ICENABLER, GICD_ICFGR, GICD_IPRIORITYR,
    GICD_ITARGETSR, GICD_TYPER, PRIVATE_LINES, V2M_COUNT, V2M_FIRST, WIDEN_SPINS,
    enabled_for_check, window,
};
use crate::arch::DistributorCheck;
use crate::mmio::Mmio;

/// Clear-pending, one bit per interrupt.
const GICD_ICPENDR: u64 = 0x280;

/// Rounds, each one race.
const ROUNDS: u32 = 64;

/// How long each core is held between its read and its write. Long enough
/// that the other core, released from the same rendezvous, has read too:
/// the rendezvous leaves them a few hundred instructions apart at most.
const WIDEN: u32 = 20_000;

/// How long anything in the check waits for a core before giving up on it.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// The two lines this round races: the first two of the chosen word.
static LINES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// The round the racers may run: they race round `n` once this is `n`.
static GO: AtomicU32 = AtomicU32::new(0);

/// Racers at the rendezvous, counted through every round: both are there
/// for round `n` once this is `2n`.
static ARRIVED: AtomicUsize = AtomicUsize::new(0);

/// Racers done, counted the same way.
static FINISHED: AtomicUsize = AtomicUsize::new(0);

/// Racers that gave up waiting for the other, or for the next round.
static STRANDED: AtomicUsize = AtomicUsize::new(0);

/// Race two cores through [`super::set_edge_triggered`] and [`super::enable`]
/// on neighbouring SPIs, [`ROUNDS`] times.
///
/// # Errors
///
/// A line that lost its priority, target or configuration; lines that did not
/// come back as they were; or a core that never reached the rendezvous.
pub(crate) fn concurrent_enables() -> Result<DistributorCheck, &'static str> {
    if crate::smp::topology().is_none_or(|cpus| cpus.online() < 2) {
        return Ok(DistributorCheck {
            skipped: Some("one processor, so nothing to race"),
            ..DistributorCheck::default()
        });
    }
    let gicd = window(&DISTRIBUTOR);
    let Some(base) = idle_word(gicd) else {
        return Ok(DistributorCheck {
            skipped: Some("no sixteen SPIs that nothing owns"),
            ..DistributorCheck::default()
        });
    };
    let (first, second) = (base, base + 1);
    LINES[0].store(first, Ordering::Relaxed);
    LINES[1].store(second, Ordering::Relaxed);

    let priority_at = GICD_IPRIORITYR + u64::from(base);
    let target_at = GICD_ITARGETSR + u64::from(base);
    let config_at = GICD_ICFGR + u64::from(base / 16) * 4;
    let saved = (
        gicd.read32(priority_at),
        gicd.read32(target_at),
        gicd.read32(config_at),
    );
    let line_bits = |line: u32| (1_u32 << (line % 32), u64::from(line / 32) * 4);
    let edge = |line: u32| 1_u32 << (2 * (line % 16) + 1);

    GO.store(0, Ordering::SeqCst);
    ARRIVED.store(0, Ordering::SeqCst);
    FINISHED.store(0, Ordering::SeqCst);
    STRANDED.store(0, Ordering::SeqCst);
    // Pinned one to each of the first two cores, so that the two changes are
    // made on two processors at once. The check itself sleeps while they
    // run, so it never holds either core from them.
    let mut racers = alloc::vec::Vec::new();
    for (index, cpu) in [0_usize, 1].into_iter().enumerate() {
        let racer = crate::sched::spawn_on(
            "gic-racer",
            racer,
            index,
            NICE_0_WEIGHT,
            cpu,
            CpuSet::of(cpu),
        )?;
        racers.push(racer);
    }

    let mut lost = 0;
    WIDEN_SPINS.store(WIDEN, Ordering::Relaxed);
    let raced = (1..=ROUNDS).try_for_each(|round| {
        // Priority and target zero for the two, both level-sensitive: what
        // neither core's change may leave behind.
        gicd.write32(priority_at, saved.0 & !0xFFFF);
        gicd.write32(target_at, saved.1 & !0xFFFF);
        gicd.write32(config_at, saved.2 & !(edge(first) | edge(second)));

        GO.store(round, Ordering::SeqCst);
        let done = 2 * round as usize;
        let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
        while FINISHED.load(Ordering::SeqCst) < done {
            if STRANDED.load(Ordering::SeqCst) != 0 || crate::timer::now_nanos() > deadline {
                return Err("a core never reached the distributor check's rendezvous");
            }
            crate::sched::sleep_for(100_000);
        }

        let priorities = gicd.read32(priority_at).to_ne_bytes();
        let targets = gicd.read32(target_at).to_ne_bytes();
        let config = gicd.read32(config_at);
        let kept = |index: usize, line: u32| {
            priorities.get(index) == Some(&DEFAULT_PRIORITY)
                && targets.get(index).is_some_and(|target| *target != 0)
                && config & edge(line) != 0
        };
        if !kept(0, first) || !kept(1, second) {
            lost += 1;
        }
        // Off again, nothing left pending.
        for line in [first, second] {
            let (bit, word) = line_bits(line);
            gicd.write32(GICD_ICENABLER + word, bit);
            gicd.write32(GICD_ICPENDR + word, bit);
        }
        Ok(())
    });
    WIDEN_SPINS.store(0, Ordering::Relaxed);
    // Past the last round, so that racers still waiting for one stop.
    GO.store(u32::MAX, Ordering::SeqCst);

    // As they were, whatever happened above.
    gicd.write32(priority_at, saved.0);
    gicd.write32(target_at, saved.1);
    gicd.write32(config_at, saved.2);
    raced?;
    for racer in &racers {
        crate::sched::wait_until_gone(racer, crate::sched::REAPER_PATIENCE_NANOS)?;
    }
    if (
        gicd.read32(priority_at),
        gicd.read32(target_at),
        gicd.read32(config_at),
    ) != saved
        || enabled_for_check(first)
        || enabled_for_check(second)
    {
        return Err("the distributor check did not leave its lines as it found them");
    }
    if lost != 0 {
        return Err(
            "two cores enabling neighbouring lines lost a line's priority, target or configuration",
        );
    }
    Ok(DistributorCheck {
        lines: Some((first, second)),
        rounds: ROUNDS,
        lost,
        skipped: None,
    })
}

/// A racer: for every round, wait for it, meet the other racer, then make
/// its line edge-triggered and enable it -- `msi_allocate`'s order, the
/// configuration changed while the line is still disabled.
fn racer(index: usize) {
    let Some(line) = LINES.get(index).map(|line| line.load(Ordering::Relaxed)) else {
        return;
    };
    for round in 1..=ROUNDS {
        if !wait_until(true, || GO.load(Ordering::SeqCst) >= round)
            || GO.load(Ordering::SeqCst) > round
        {
            return;
        }
        let _ = ARRIVED.fetch_add(1, Ordering::SeqCst);
        let both = 2 * round as usize;
        if !wait_until(false, || ARRIVED.load(Ordering::SeqCst) >= both) {
            return;
        }
        super::set_edge_triggered(line);
        super::enable(line);
        let _ = FINISHED.fetch_add(1, Ordering::SeqCst);
    }
}

/// Wait until `ready`, or give up after [`PATIENCE_NANOS`] and say so in
/// [`STRANDED`]. Yielding while a round is awaited, so that the check task,
/// which may share this core, gets it; spinning at the rendezvous, so that
/// the two racers leave it within instructions of each other.
fn wait_until(yielding: bool, ready: impl Fn() -> bool) -> bool {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !ready() {
        if crate::timer::now_nanos() > deadline {
            let _ = STRANDED.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        if yielding {
            crate::sched::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }
    true
}

/// The first line of the highest configuration word -- sixteen SPIs, and so
/// four whole priority and target words -- that nothing owns: no device node
/// names any of its lines, no handler is registered on one, the `GICv2m` frame
/// hands none out, and none is enabled.
fn idle_word(gicd: Mmio) -> Option<u32> {
    let lines = ((gicd.read32(GICD_TYPER) & 0b1_1111) + 1) * 32;
    let limit = lines.min(FIRST_SPECIAL_ID);
    let v2m_first = V2M_FIRST.load(Ordering::Relaxed);
    let v2m = v2m_first..v2m_first + V2M_COUNT.load(Ordering::Relaxed);
    let described = |line: u32| {
        crate::device::devices().iter().any(|node| {
            (0..node.vector_count())
                .filter_map(|index| node.vector(index))
                .any(|vector| vector.number() == line)
        })
    };
    let owned = |line: u32| {
        v2m.contains(&line)
            || enabled_for_check(line)
            || crate::irq::is_registered(line)
            || described(line)
    };
    (PRIVATE_LINES as u32..limit)
        .step_by(16)
        .rev()
        .find(|&base| base + 16 <= limit && !(base..base + 16).any(owned))
}
