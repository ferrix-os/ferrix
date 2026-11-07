//! A window's unmap (`AddressSpace::window_take_down`, `docs/microkernel/M5-LIFT.md` §13,
//! ledger 6421): what it takes down, what it asks to be flushed, and when it begins a
//! shootdown at all. Read by the decision, never by timing. That another processor's
//! translation really goes is `test-threads`' stale read (its `unmap-stale` case).

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use super::{AddressSpace, PageRange, Vmo};
use crate::user::vmo;
use ferrix_bootinfo::PAGE_SIZE;

/// Where the made-up window's pages sit: inside one 2 MiB table, at its start.
const BASE: u64 = 0x10_0000_0000;
/// A range a walk page by page could not finish in any stated time: 64 TiB, less the top.
const HUGE: u64 = 0x4000_0000_0000;

/// How many decisions were checked.
pub fn run() -> Result<u32, &'static str> {
    let region = AddressSpace::new_region().map_err(|_| "no region")?;
    let space = AddressSpace::new().map_err(|_| "no space")?;
    space
        .bind_region(&region, crate::sched::running_group())
        .map_err(|_| "not bound")?;
    space.open_window().map_err(|_| "no window")?;
    let memory = Vmo::new_anonymous(64).map_err(|_| "no object")?;
    let mut cases = 0;

    // Nothing there: within the ceiling and over it, no shootdown begins.
    let begun = space.shootdowns_begun();
    for pages in [4, 64] {
        let (flush, cpus) = take_down(&space, BASE, pages)?;
        if !flush.is_empty() || cpus.is_some() || space.shootdowns_begun() != begun {
            return Err("a range with nothing in it began a shootdown");
        }
        cases += 1;
    }

    // A huge range with nothing in it comes back at all (holes stepped whole), and one
    // with a leaf at its far end takes that leaf down and names it.
    let far = BASE + HUGE - PAGE_SIZE;
    place(&space, &memory, &[far])?;
    let range = PageRange::from_len(BASE, HUGE).map_err(|_| "a huge range")?;
    let (mut flush, cpus) = space.window_take_down(range);
    if flush.addresses() != [far] || mm_present(&space, far) {
        return Err("a huge range's far leaf was not the one thing named");
    }
    finish(&space, cpus, &mut flush);
    cases += 1;

    // Leaf, none, leaf: exactly two named, and one shootdown.
    let (first, last) = (BASE, BASE + 2 * PAGE_SIZE);
    place(&space, &memory, &[first, last])?;
    let begun = space.shootdowns_begun();
    let (mut flush, cpus) = take_down(&space, BASE, 3)?;
    if flush.addresses() != [first, last] || space.shootdowns_begun() != begun + 1 {
        return Err("leaf, none, leaf did not name the two leaves in one shootdown");
    }
    if mm_present(&space, first) || mm_present(&space, last) {
        return Err("a named leaf is still there");
    }
    finish(&space, cpus, &mut flush);
    cases += 1;

    // Over the ceiling with five leaves: five named, not the whole TLB.
    let five: [u64; 5] = core::array::from_fn(|index| BASE + 10 * index as u64 * PAGE_SIZE);
    place(&space, &memory, &five)?;
    let (mut flush, cpus) = take_down(&space, BASE, 64)?;
    if flush.is_everything() || flush.addresses() != five {
        return Err("five leaves in a long range were not the five named");
    }
    finish(&space, cpus, &mut flush);
    cases += 1;

    // Past the ceiling in leaves: the whole TLB.
    let many: [u64; 34] = core::array::from_fn(|index| BASE + index as u64 * PAGE_SIZE);
    place(&space, &memory, &many)?;
    let (mut flush, cpus) = take_down(&space, BASE, 64)?;
    if !flush.is_everything() {
        return Err("34 leaves did not ask for the whole TLB");
    }
    finish(&space, cpus, &mut flush);
    cases += 1;

    // Another call's shootdown still out: everything, leaf or no leaf, and one begins.
    let _ = space.flushes.pending.fetch_add(1, Ordering::SeqCst);
    place_held(&space, &memory, first)?;
    let (mut flush, cpus) = take_down(&space, BASE, 2)?;
    let with_a_leaf = flush.is_everything();
    finish(&space, cpus, &mut flush);
    let begun = space.shootdowns_begun();
    let (mut flush, cpus) = take_down(&space, BASE, 2)?;
    let with_none = flush.is_everything() && space.shootdowns_begun() == begun + 1;
    finish(&space, cpus, &mut flush);
    let _ = space.flushes.pending.fetch_sub(1, Ordering::SeqCst);
    if !with_a_leaf {
        return Err("a shootdown still out was not covered, with a leaf taken");
    }
    if !with_none {
        return Err("a shootdown still out was not covered, with nothing taken");
    }
    cases += 2;

    Ok(cases)
}

/// `pages` pages from `at`, down.
fn take_down(
    space: &AddressSpace,
    at: u64,
    pages: u64,
) -> Result<(crate::smp::TlbPages, Option<ferrix_sched_class::CpuSet>), &'static str> {
    let range = PageRange::from_len(at, pages * PAGE_SIZE).map_err(|_| "a range")?;
    Ok(space.window_take_down(range))
}

/// What `unmap` does after the decision.
fn finish(
    space: &AddressSpace,
    cpus: Option<ferrix_sched_class::CpuSet>,
    flush: &mut crate::smp::TlbPages,
) {
    if let Some(cpus) = cpus {
        space.shoot(&cpus, flush);
    }
}

/// Whether a leaf stands at `at`.
fn mm_present(space: &AddressSpace, at: u64) -> bool {
    crate::mm::translate_in(space.root_table(), at).is_some()
}

/// Place one page of `memory` at each of `pages`.
fn place(space: &AddressSpace, memory: &Arc<Vmo>, pages: &[u64]) -> Result<(), &'static str> {
    for (index, &at) in pages.iter().enumerate() {
        let (done, refused) = space.place(
            memory,
            true,
            false,
            &[(at, index as u64 * PAGE_SIZE, true, false)],
        );
        if done != 1 || refused.is_some() {
            return Err("a page was not placed");
        }
    }
    Ok(())
}

/// [`place`], while a test holds `pending` up, which a place does not read.
fn place_held(space: &AddressSpace, memory: &Arc<Vmo>, at: u64) -> Result<(), &'static str> {
    place(space, memory, &[at])
}
