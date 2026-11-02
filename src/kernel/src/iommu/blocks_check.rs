//! Stage 10's check of 2 MiB VT-d entries for a pin's aligned runs
//! (`docs/INTEL-GPU.md` §14.7, ledger 5366): the unit's bit is the one
//! `CAP` reads, an aligned run of 512 is one 2 MiB entry and nothing else
//! is, `blocks` false maps every page at 4 KiB, a run meeting another pin
//! is refused with that pin left as it was, and an unpin or an unwind takes
//! each run down whole and gives back every table it made.
//!
//! On a domain that is not a VT-d unit's, or on a unit without `CAP.SLLPS`
//! 2 MiB, the run cases are not reached and the line says so: QEMU's
//! intel-iommu always offers the bit, so the arm without it rests on
//! `accept`'s host test and on the `blocks` false case here.

#![allow(
    clippy::disallowed_methods,
    reason = "a raised check: it calls methods made pub for it alone"
)]

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::MapFlags;

use crate::mm;

use super::{Domain, DomainError, Pinned};

/// A 2 MiB entry's span.
const RUN: u64 = 2 << 20;
/// Its pages.
const RUN_PAGES: usize = (RUN / PAGE_SIZE) as usize;
/// The order of the frames the cases pin: two runs, 4 MiB aligned.
const ORDER: u8 = 10;

/// Stage 10: the run cases on `domain`, the first PCI function's.
///
/// # Errors
///
/// The first case whose rule did not hold.
///
/// Verifies: L.iommu.106, L.iommu.107, L.iommu.108, H.DMA.3
pub(super) fn check_runs(domain: &Domain) -> Result<Option<String>, &'static str> {
    let Some((blocks, offered)) = domain.blocks_wiring() else {
        return Ok(None);
    };
    if blocks != offered {
        return Err("a VT-d domain's blocks are not what its unit's CAP.SLLPS offers");
    }
    if !blocks {
        return Ok(Some(String::from(
            "runs: the unit offers no 2 MiB pages, every pin page by page",
        )));
    }
    let Some(first) = mm::allocate_frames(ORDER) else {
        return Err("no 2 MiB-aligned frames for the run check");
    };
    let frames: Vec<u64> = (first..first + (1 << ORDER)).collect();
    let result = cases(domain, &frames);
    mm::deallocate_frames(first, ORDER);
    result.map(|()| {
        Some(String::from(
            "runs: two aligned runs pinned as 2 MiB entries and a broken one page by \
             page, every page 4 KiB without blocks, a run over an empty table page by page, \
             a run over another pin refused with it intact, each taken down whole with every \
             table given back",
        ))
    })
}

/// The cases, over `frames`: 1024 frames from a 4 MiB boundary.
fn cases(domain: &Domain, frames: &[u64]) -> Result<(), &'static str> {
    // Once unmeasured, so that what a first pin of a region makes for good
    // -- the heap's room for the address list -- is not counted as a table.
    let warm = domain
        .pin(frames, MapFlags::DMA)
        .map_err(|_| "a domain refused a pin of two runs")?;
    if domain.unpin(warm).is_err() {
        return Err("a domain refused to unpin a pin of runs");
    }
    both_runs_are_blocks(domain, frames)?;
    a_broken_run_is_pages(domain, frames)?;
    without_blocks_every_page_is_4k(domain, frames)?;
    a_run_over_a_table_is_pages(domain, frames)?;
    a_run_over_a_pinned_page_is_refused(domain, frames)?;
    a_blocked_window_is_refused(domain, frames)?;
    an_unwind_takes_a_block_down(domain, frames)
}

/// The `n`th of `frames`: the cases ask only for frames of the 1024 they
/// were given, so the 0 past the end is never asked for.
fn frame(frames: &[u64], n: usize) -> u64 {
    frames.get(n).copied().unwrap_or(0)
}

/// The device address of frame `frame`.
fn address(frame: u64) -> u64 {
    frame * PAGE_SIZE
}

/// Whether every one of `frames` resolves to itself in `domain`.
fn all_resolve(domain: &Domain, frames: &[u64]) -> bool {
    frames
        .iter()
        .all(|&frame| domain.resolve(address(frame)) == Some(address(frame)))
}

/// Whether none of `frames` resolves in `domain`.
fn none_resolve(domain: &Domain, frames: &[u64]) -> bool {
    frames
        .iter()
        .all(|&frame| domain.resolve(address(frame)).is_none())
}

/// Unpin `pinned`, and require that none of `frames` resolves after and that
/// every table the pin made is given back: the free count is `free` again.
/// Each case allocates what it needs before it reads `free`, so the heap's
/// growth is not taken for a kept table.
fn unpin_all(
    domain: &Domain,
    pinned: Pinned,
    frames: &[u64],
    free: u64,
) -> Result<(), &'static str> {
    if domain.unpin(pinned).is_err() {
        return Err("a domain refused to unpin a pin of runs");
    }
    if !none_resolve(domain, frames) {
        return Err("a page of an unpinned run still resolved");
    }
    if mm::free_frames() != free {
        return Err("an unpin of runs kept a table it emptied");
    }
    Ok(())
}

/// `L.iommu.106`, `L.iommu.108`: 1024 pages from a 4 MiB boundary are two
/// 2 MiB entries, every page resolves, and the unpin takes both down.
fn both_runs_are_blocks(domain: &Domain, frames: &[u64]) -> Result<(), &'static str> {
    let free = mm::free_frames();
    let pinned = domain
        .pin(frames, MapFlags::DMA)
        .map_err(|_| "a domain refused a pin of two runs")?;
    let blocked = frames.chunks(RUN_PAGES).all(|run| {
        run.iter()
            .all(|&frame| domain.entry_span(address(frame)) == Some(RUN))
    });
    if !blocked || !all_resolve(domain, frames) {
        pinned.leak();
        return Err("a pin's aligned runs were not 2 MiB entries reaching their frames");
    }
    unpin_all(domain, pinned, frames, free)
}

/// `L.iommu.106`: the first run less its first page is 511 pages at 4 KiB,
/// the second still one entry: a run starts only at a 2 MiB boundary,
/// though 512 of the pin's pages run on from its first.
fn a_broken_run_is_pages(domain: &Domain, frames: &[u64]) -> Result<(), &'static str> {
    let free = mm::free_frames();
    let pinned_frames = frames.get(1..).unwrap_or_default();
    let pinned = domain
        .pin(pinned_frames, MapFlags::DMA)
        .map_err(|_| "a domain refused a pin of a broken run and a whole one")?;
    let (broken, run) = pinned_frames.split_at(RUN_PAGES - 1);
    let right = broken
        .iter()
        .all(|&frame| domain.entry_span(address(frame)) == Some(PAGE_SIZE))
        && run
            .iter()
            .all(|&frame| domain.entry_span(address(frame)) == Some(RUN))
        && domain.resolve(address(frame(frames, 0))).is_none();
    if !right || !all_resolve(domain, pinned_frames) {
        pinned.leak();
        return Err("a broken run was not mapped page by page beside a whole one");
    }
    unpin_all(domain, pinned, frames, free)
}

/// `L.iommu.106`, `L.iommu.108`: with `blocks` false, as a unit without
/// `CAP.SLLPS` maps, every page is 4 KiB, and the unpin's take-down of the
/// runs as 2 MiB ranges takes 512 pages each.
fn without_blocks_every_page_is_4k(domain: &Domain, frames: &[u64]) -> Result<(), &'static str> {
    let free = mm::free_frames();
    let pinned = domain
        .pin_runs(frames, MapFlags::DMA, false)
        .map_err(|_| "a domain refused a pin without blocks")?;
    let pages = frames
        .iter()
        .all(|&frame| domain.entry_span(address(frame)) == Some(PAGE_SIZE));
    if !pages || !all_resolve(domain, frames) {
        pinned.leak();
        return Err("a pin without blocks mapped a 2 MiB entry");
    }
    unpin_all(domain, pinned, frames, free)
}

/// `L.iommu.107`: a run whose slot holds an empty table, which no product
/// path leaves, is mapped whole as 512 pages of 4 KiB, and its unpin's
/// 2 MiB take-down prunes the table with them.
fn a_run_over_a_table_is_pages(domain: &Domain, frames: &[u64]) -> Result<(), &'static str> {
    let run = frames.get(..RUN_PAGES).unwrap_or_default();
    let free = mm::free_frames();
    domain
        .link_empty_table(address(frame(run, 0)))
        .map_err(|_| "no empty table could be left in a window")?;
    if domain.resolve(address(frame(run, 0))).is_some() {
        return Err("the empty table left in a window still mapped its page");
    }
    let pinned = domain
        .pin(run, MapFlags::DMA)
        .map_err(|_| "a run over an empty table was refused, not mapped page by page")?;
    let pages = run
        .iter()
        .all(|&frame| domain.entry_span(address(frame)) == Some(PAGE_SIZE));
    if !pages || !all_resolve(domain, run) {
        pinned.leak();
        return Err("a run over an empty table was not mapped page by page");
    }
    unpin_all(domain, pinned, run, free)
}

/// `L.iommu.107`, `L.iommu.108`: a run over a window holding another pin's
/// page falls back to pages, is refused at that page with `AlreadyPinned`,
/// and its unwind takes out its own pages one by one: the other pin's page
/// still resolves.
fn a_run_over_a_pinned_page_is_refused(
    domain: &Domain,
    frames: &[u64],
) -> Result<(), &'static str> {
    let run = frames.get(..RUN_PAGES).unwrap_or_default();
    let other = frame(run, 5);
    let own: Vec<u64> = run
        .iter()
        .copied()
        .filter(|&frame| frame != other)
        .collect();
    let free = mm::free_frames();
    let held = domain
        .pin(&[other], MapFlags::DMA)
        .map_err(|_| "a domain refused a pin of one page")?;
    let before = domain.pinned_pages();
    let refused = matches!(
        domain.pin(run, MapFlags::DMA),
        Err(DomainError::AlreadyPinned)
    );
    if !refused
        || domain.pinned_pages() != before
        || !none_resolve(domain, &own)
        || domain.entry_span(address(other)) != Some(PAGE_SIZE)
    {
        held.leak();
        return Err("a run over another pin's page was not refused with that pin left as it was");
    }
    unpin_all(domain, held, run, free)
}

/// `L.iommu.107`: a window one block maps refuses a second pin of the run
/// and a pin of one page inside it, each `AlreadyPinned`, and the block
/// stays whole.
fn a_blocked_window_is_refused(domain: &Domain, frames: &[u64]) -> Result<(), &'static str> {
    let run = frames.get(..RUN_PAGES).unwrap_or_default();
    let free = mm::free_frames();
    let pinned = domain
        .pin(run, MapFlags::DMA)
        .map_err(|_| "a domain refused a pin of one run")?;
    let again = domain.pin(run, MapFlags::DMA);
    let inside = domain.pin(&[frame(run, 7)], MapFlags::DMA);
    let refused = matches!(again, Err(DomainError::AlreadyPinned))
        && matches!(inside, Err(DomainError::AlreadyPinned));
    let whole = run
        .iter()
        .all(|&frame| domain.entry_span(address(frame)) == Some(RUN));
    if !refused || !whole || !all_resolve(domain, run) {
        for pin in [again, inside].into_iter().flatten() {
            pin.leak();
        }
        pinned.leak();
        return Err("a blocked window took a second pin or lost its block");
    }
    unpin_all(domain, pinned, run, free)
}

/// `L.iommu.108`: a pin refused after its first run was mapped as a block
/// takes the block down whole in its unwind.
fn an_unwind_takes_a_block_down(domain: &Domain, frames: &[u64]) -> Result<(), &'static str> {
    let run = frames.get(..RUN_PAGES).unwrap_or_default();
    let other = frame(frames, RUN_PAGES + 88);
    let mut asked: Vec<u64> = run.to_vec();
    asked.push(other);
    let free = mm::free_frames();
    let held = domain
        .pin(&[other], MapFlags::DMA)
        .map_err(|_| "a domain refused a pin of one page")?;
    let refused = matches!(
        domain.pin(&asked, MapFlags::DMA),
        Err(DomainError::AlreadyPinned)
    );
    if !refused || !none_resolve(domain, run) || domain.resolve(address(other)).is_none() {
        held.leak();
        return Err("the unwind of a refused pin left its block mapped");
    }
    unpin_all(domain, held, &[other], free)
}
