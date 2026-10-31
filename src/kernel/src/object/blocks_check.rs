//! `vmo_pin`'s `PIN_BLOCKS` (`docs/INTEL-GPU.md` §14.3; the consultant's
//! review, ledger 5088), proved at stage 10 on a PCI function's node.
//!
//! The cases, each read from the object's frames and the job's charge
//! rather than from what the call answered:
//! 1. the option word fails closed: an order without `PIN_BLOCKS`,
//!    `PIN_BLOCKS` without an order or past the allocator's largest block,
//!    and `PIN_BLOCKS` with `PIN_CONTIGUOUS`, each `INVALID_ARGS` with
//!    nothing committed;
//! 2. a pin past the device's budget is `LIMIT_REACHED` with nothing
//!    committed: the budget is read before any frame is taken;
//! 3. over a range with one page committed, the blocks are naturally
//!    aligned in the object and in memory, at most the asked order, only
//!    over absent pages, and the committed page keeps its frame;
//! 4. with every block of order 5 and up taken from the allocator, the
//!    range is still committed whole, and no order the allocator refused is
//!    asked again;
//! 5. a page committed between a block's probe and its commit (the
//!    [`BlockHook`], so the race happens every time) keeps its frame, and
//!    the rest is committed around it;
//! 6. a pin refused at its job's memory limit leaves exactly the pages it
//!    committed committed, charged to the job, and nothing else;
//! 7. a file's object is not committed at all;
//! 8. a pin refused at its job's memory limit, with clean file cache charged
//!    to the job, takes the cache back and is answered (`L.object.257`).
//!
//! Every case asks [`Vmo::commit_run`] at most `pages + MAX_ORDER + 1`
//! times.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_frame::{Frame, MAX_ORDER};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{PIN_BLOCK_ORDER_SHIFT, PIN_BLOCKS, PIN_CONTIGUOUS};

use crate::device::DeviceNode;
use crate::mm;
use crate::object::check::{Side, device_handle, reg};
use crate::object::job::Job;
use crate::object::quota::Resource;
use crate::object::{self, Object};
use crate::sched;
use crate::user::vmo::{self, BlockHook, Filler, Vmo};

/// The order the checks ask for: 2 MiB at 4 KiB pages, what the Intel
/// driver asks.
const ORDER: u8 = 9;
/// Pages in one such block.
const BLOCK: u64 = 1 << ORDER;

/// Run every case on `node`; the calls refused as they had to be.
///
/// Verifies: L.object.254, L.object.255, L.object.256, L.object.257, L.object.258
pub(crate) fn run(node: &Arc<DeviceNode>) -> Result<u32, &'static str> {
    let side = Side::new()?;
    let device = device_handle(&side, node)?;
    let mut refusals = check_the_option_word(&side, device)?;
    refusals += check_the_budget_first(&side, device, node)?;
    check_blocks_over_absent_pages(&side, device)?;
    check_the_fallback()?;
    check_the_race()?;
    refusals += check_the_failure_rule(&side, device)?;
    check_a_file_is_not_committed()?;
    check_cache_makes_room(&side, device)?;
    side.close_everything();
    Ok(refusals)
}

/// `options` for `PIN_BLOCKS` of `order`.
const fn blocks(order: u64) -> u64 {
    PIN_BLOCKS | order << PIN_BLOCK_ORDER_SHIFT
}

/// `vmo` in `side`'s table, with a VMO's rights.
fn vmo_handle(side: &Side, vmo: &Arc<Vmo>) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Vmo(Arc::clone(vmo)), Rights::VMO))
        .map_err(|_| "no room for a VMO handle")
}

/// `vmo_pin` of `pages` pages from 0 with `options`.
fn pin(
    side: &Side,
    device: Handle,
    vmo: Handle,
    pages: u64,
    options: u64,
) -> Result<usize, ferrix_linux_abi::errno::Errno> {
    side.call(
        nr::VMO_PIN,
        &[reg(device), reg(vmo), 0, pages * PAGE_SIZE, options],
    )
}

/// Close `handle` in `side`.
fn close(side: &Side, handle: usize) -> Result<(), &'static str> {
    side.call(nr::HANDLE_CLOSE, &[handle as u64])
        .map(|_| ())
        .map_err(|_| "closing a handle failed")
}

/// What [`Vmo::commit_blocks`] said: its asks of `commit_run`, and the blocks
/// that went in.
type Blocks = (u64, u64);

/// The asks of `done` over `pages` pages are within the bound.
fn within_bound((attempts, blocks): Blocks, pages: u64) -> Result<(), &'static str> {
    if attempts > pages + u64::from(MAX_ORDER) + 1 || blocks > attempts {
        return Err("PIN_BLOCKS asked the allocator more often than its bound");
    }
    Ok(())
}

/// Case 1, `L.object.254`.
fn check_the_option_word(side: &Side, device: Handle) -> Result<u32, &'static str> {
    let vmo = Vmo::new_anonymous(2).map_err(|_| "no memory for a VMO")?;
    let handle = vmo_handle(side, &vmo)?;
    let order_alone = u64::from(ORDER) << PIN_BLOCK_ORDER_SHIFT;
    let past = blocks(u64::from(MAX_ORDER) + 1);
    let mut refusals = 0;
    for options in [
        order_alone,
        PIN_BLOCKS,
        past,
        blocks(u64::from(ORDER)) | PIN_CONTIGUOUS,
    ] {
        if pin(side, device, handle, 2, options) != Err(status::INVALID_ARGS) {
            return Err("vmo_pin took a PIN_BLOCKS option word it must refuse");
        }
        refusals += 1;
    }
    if vmo.committed() != 0 {
        return Err("a refused PIN_BLOCKS option word committed pages");
    }
    Ok(refusals)
}

/// Case 2, `L.object.257`'s budget half.
fn check_the_budget_first(
    side: &Side,
    device: Handle,
    node: &Arc<DeviceNode>,
) -> Result<u32, &'static str> {
    let domain = node
        .domain()
        .map_err(|_| "no memory for a device's domain")?;
    let budget = object::pin::counts(&domain).budget as u64;
    let vmo = Vmo::new_anonymous(budget + 1).map_err(|_| "no memory for a VMO")?;
    let handle = vmo_handle(side, &vmo)?;
    if pin(side, device, handle, budget + 1, blocks(u64::from(ORDER))) != Err(status::LIMIT_REACHED)
    {
        return Err("a PIN_BLOCKS pin past its device's budget was not LIMIT_REACHED");
    }
    if vmo.committed() != 0 {
        return Err("a PIN_BLOCKS pin past the budget committed pages before it was refused");
    }
    Ok(1)
}

/// Whether the object's pages `first..first + len` are one run of frames
/// from a frame that is a multiple of `len`.
fn is_block(vmo: &Vmo, first: u64, len: u64) -> bool {
    let Some(start) = vmo.page(first) else {
        return false;
    };
    start.is_multiple_of(len) && (0..len).all(|n| vmo.page(first + n) == Some(start + n))
}

/// Case 3, `L.object.255`: two blocks' worth, page 3 committed first. The
/// first block's pages around page 3 go in as the largest blocks their
/// places allow: 0-1, 2, then 4, 8, ... 256; the second block is one.
fn check_blocks_over_absent_pages(side: &Side, device: Handle) -> Result<(), &'static str> {
    let vmo = Vmo::new_anonymous(2 * BLOCK).map_err(|_| "no memory for a VMO")?;
    let three = vmo
        .commit(3)
        .map_err(|_| "committing a page before PIN_BLOCKS failed")?;
    let handle = vmo_handle(side, &vmo)?;
    let pinned = pin(side, device, handle, 2 * BLOCK, blocks(u64::from(ORDER)))
        .map_err(|_| "a PIN_BLOCKS pin over absent pages was refused")?;
    if vmo.page(3) != Some(three) {
        return Err("PIN_BLOCKS replaced a page that was committed before it");
    }
    let mut expected: Vec<(u64, u64)> = Vec::new();
    expected.extend([(0, 2), (2, 1)]);
    expected.extend((2..u64::from(ORDER)).map(|o| (1 << o, 1 << o)));
    expected.push((BLOCK, BLOCK));
    if !expected
        .iter()
        .all(|&(first, len)| is_block(&vmo, first, len))
    {
        return Err("PIN_BLOCKS's pages were not naturally aligned blocks of the largest order");
    }
    // At most the order asked: the two halves are not one 4 MiB block.
    if is_block(&vmo, BLOCK, 2 * BLOCK) && is_block(&vmo, 0, 2 * BLOCK) {
        return Err("PIN_BLOCKS took a block above the order asked");
    }
    close(side, pinned)
}

/// Frames held so that no free block of 32 frames or more is left: one in
/// every 32, given back when dropped.
struct Taken(Vec<Frame>);

impl Drop for Taken {
    fn drop(&mut self) {
        for &frame in &self.0 {
            let _ = mm::release_frame(frame);
        }
    }
}

/// Case 4, `L.object.256`: with no free block of order 5 or more, a
/// 2 MiB-aligned range asked at order 9 is still committed whole, and each
/// order from 9 down to 5 is refused once at most. Every large block is
/// taken, split, and given back but for one frame in each 32, so the free
/// count stays high (above the program watermark) and no buddy of 32 can
/// form again. It runs at boot, at stage 10, and holds at most free/32
/// frames, all given back before it returns.
fn check_the_fallback() -> Result<(), &'static str> {
    const KEEP: u64 = 32;
    let vmo = Vmo::new_anonymous(BLOCK).map_err(|_| "no memory for a VMO")?;
    // Room for every kept frame before any is kept, so the pushes take none.
    let mut taken = Taken(Vec::new());
    let room = (mm::free_frames() / KEEP) as usize + 1;
    taken
        .0
        .try_reserve_exact(room)
        .map_err(|_| "no memory to record the frames kept")?;
    for order in (5..=MAX_ORDER).rev() {
        while taken.0.len() + (1 << order) / KEEP as usize <= room {
            let Some(block) = mm::allocate_frames(order) else {
                break;
            };
            if !mm::split_frames(block, order) {
                mm::deallocate_frames(block, order);
                return Err("a block taken for the PIN_BLOCKS fallback could not be split");
            }
            for frame in block..block + (1 << order) {
                if frame % KEEP == KEEP / 2 {
                    taken.0.push(frame);
                } else {
                    let _ = mm::release_frame(frame);
                }
            }
        }
    }
    let done = vmo.commit_blocks(0, BLOCK, ORDER);
    drop(taken);
    if vmo.committed() != BLOCK as usize {
        return Err("PIN_BLOCKS with no large block free left pages absent");
    }
    within_bound(done, BLOCK)?;
    // The fallback ran: an order was refused, and nothing of 32 pages or
    // more went in as one aligned block.
    let (attempts, blocks) = done;
    if attempts <= blocks {
        return Err("PIN_BLOCKS with no large block free was never refused a block");
    }
    if (0..BLOCK / KEEP).any(|n| is_block(&vmo, n * KEEP, KEEP)) {
        return Err("PIN_BLOCKS committed a block of 32 with none free");
    }
    // Each refusal lowers the order for good: five refusals at most (9 to
    // 5), whatever the blocks of 16 and less.
    if attempts > blocks + u64::from(ORDER - 4) {
        return Err("PIN_BLOCKS asked again for an order the allocator had refused");
    }
    Ok(())
}

/// The page the race commits, and its frame once it has.
const RACED: u64 = 100;
static RACED_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

/// The race: once, at the first block's commit, page [`RACED`] goes in.
fn race(vmo: &Vmo, first: u64, pages: u64) {
    if (first..first + pages).contains(&RACED)
        && RACED_FRAME.load(Ordering::SeqCst) == u64::MAX
        && let Ok(frame) = vmo.commit(RACED)
    {
        RACED_FRAME.store(frame, Ordering::SeqCst);
    }
}

static RACE: BlockHook = BlockHook {
    armed_by: "object::blocks_check's race",
    run: race,
};

/// Case 5, `L.object.256`'s race.
fn check_the_race() -> Result<(), &'static str> {
    let vmo = Vmo::new_anonymous(BLOCK).map_err(|_| "no memory for a VMO")?;
    RACED_FRAME.store(u64::MAX, Ordering::SeqCst);
    vmo::arm_block_hook(&RACE);
    let done = vmo.commit_blocks(0, BLOCK, ORDER);
    vmo::disarm_block_hook();
    let raced = RACED_FRAME.load(Ordering::SeqCst);
    if raced == u64::MAX {
        return Err("the PIN_BLOCKS race's hook never ran");
    }
    if vmo.page(RACED) != Some(raced) || vmo.committed() != BLOCK as usize {
        return Err("a page committed during PIN_BLOCKS was replaced or its neighbours left out");
    }
    within_bound(done, BLOCK)
}

/// Case 6, `L.object.257`: a job whose limit is half the range.
fn check_the_failure_rule(side: &Side, device: Handle) -> Result<u32, &'static str> {
    let tree = Job::new_root().map_err(|_| "no memory for a job")?;
    let job = tree.new_child().map_err(|_| "a job refused a child")?;
    let _ = job.set_limit(Resource::Memory, BLOCK / 2 * PAGE_SIZE);
    let vmo = Vmo::new_anonymous(BLOCK).map_err(|_| "no memory for a VMO")?;
    let handle = vmo_handle(side, &vmo)?;
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let answer = pin(side, device, handle, BLOCK, blocks(u64::from(ORDER)));
    sched::set_current_group(own);
    if answer != Err(status::NO_MEMORY) {
        return Err("a PIN_BLOCKS pin past its job's memory limit was not NO_MEMORY");
    }
    let used = |resource| job.usage(resource).map_or(0, |usage| usage.used);
    let charged = used(Resource::Memory).saturating_sub(used(Resource::Kernel)) / PAGE_SIZE;
    if vmo.committed() == 0 || charged != vmo.committed() as u64 {
        return Err("a refused PIN_BLOCKS pin's committed pages and its job's charge differ");
    }
    close(side, handle.0 as usize)?;
    drop(vmo);
    if used(Resource::Memory).saturating_sub(used(Resource::Kernel)) != 0 {
        return Err("a refused PIN_BLOCKS pin's pages stayed charged after the object went");
    }
    Ok(1)
}

/// A file's source that has nothing to give.
#[derive(Debug)]
struct Nothing;

impl Filler for Nothing {
    fn fill(&self, _: &Vmo, _: u64) -> Result<(), ferrix_vfs::Errno> {
        Ok(())
    }

    fn fill_one(&self, _: &Vmo, _: u64) -> Result<(), ferrix_vfs::Errno> {
        Ok(())
    }
}

/// Case 7, `L.object.258`.
fn check_a_file_is_not_committed() -> Result<(), &'static str> {
    let source: Arc<dyn Filler> =
        crate::fallible::try_arc(Nothing).map_err(|_| "no memory for a source")?;
    let vmo = Vmo::new_filled(BLOCK, Some(source)).map_err(|_| "no memory for a VMO")?;
    let done = vmo.commit_blocks(0, BLOCK, ORDER);
    if done != (0, 0) || vmo.committed() != 0 {
        return Err("PIN_BLOCKS committed pages of a file's object");
    }
    Ok(())
}

/// A file's source whose every page can be read again: what reclaim may
/// take, as a read-only mount's pages.
#[derive(Debug)]
struct ReadAgain;

impl Filler for ReadAgain {
    fn fill(&self, _: &Vmo, _: u64) -> Result<(), ferrix_vfs::Errno> {
        Ok(())
    }

    fn fill_one(&self, _: &Vmo, _: u64) -> Result<(), ferrix_vfs::Errno> {
        Ok(())
    }

    fn reclaimable(&self) -> bool {
        true
    }
}

/// Case 8, `L.object.257`: a job filled to its memory limit with clean file
/// cache pins a block of fresh memory all the same, the cache given back to
/// make room, as a fault would take it (2026-10-31: GPU objects pinned
/// whole answered `ENOMEM` with gigabytes of cache to give back).
fn check_cache_makes_room(side: &Side, device: Handle) -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "no memory for a job")?;
    let job = tree.new_child().map_err(|_| "a job refused a child")?;
    let _ = job.set_limit(Resource::Memory, 2 * BLOCK * PAGE_SIZE);
    let source: Arc<dyn Filler> =
        crate::fallible::try_arc(ReadAgain).map_err(|_| "no memory for a source")?;
    let file = Vmo::new_filled(4 * BLOCK, Some(source)).map_err(|_| "no memory for a VMO")?;
    let vmo = Vmo::new_anonymous(BLOCK).map_err(|_| "no memory for a VMO")?;
    let handle = vmo_handle(side, &vmo)?;
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    // The cache, until the limit refuses a page of it.
    let cached = (0..4 * BLOCK)
        .take_while(|&index| file.commit_and_write(index, 0, &[1]).is_ok())
        .count() as u64;
    let answer = pin(side, device, handle, BLOCK, blocks(u64::from(ORDER)));
    sched::set_current_group(own);
    if cached == 0 || cached >= 4 * BLOCK {
        return Err("the cache check's file was not cut short by its job's memory limit");
    }
    let pinned = answer.map_err(
        |_| "a pin at its job's memory limit was refused with the job's clean cache to give back",
    )?;
    if vmo.committed() as u64 != BLOCK || file.committed() as u64 >= cached {
        return Err("a pin at its job's memory limit was answered without the cache's room");
    }
    close(side, pinned)?;
    close(side, handle.0 as usize)?;
    drop((vmo, file));
    Ok(())
}
