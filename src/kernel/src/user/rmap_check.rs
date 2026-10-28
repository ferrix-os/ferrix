//! The reverse map and the scoped shootdown, checked from user mode.
//!
//! What a VMO taking a page away has to get right cannot be seen in page
//! tables. A table with the entry gone proves the kernel took it down; it
//! proves nothing about a processor that cached the entry before, and under
//! hardware virtualisation that cache is a real TLB that keeps answering until
//! somebody tells it not to. So the accesses that matter here are made by
//! programs, in user mode, on processors other than the one doing the taking.
//!
//! Two processes share one `MAP_SHARED | MAP_ANONYMOUS` object across a fork:
//! the parent pinned to the processor this check runs on, the child to
//! another. Page 0 of the object is the page under test; page 1 is a control
//! page, on which the kernel writes each program a command and the program
//! writes back what it saw. Each program touches page 0 until told to hold
//! still — reading the control page only, so its TLB entry for page 0 stays
//! warm — and then the kernel, on this processor:
//!
//! 1. decommits page 0, requires both spaces' tables to have lost it, claims
//!    back the very frame it gave up and fills it with a poison pattern, and
//!    has the child read and write page 0: it must read zeros, not poison, and
//!    its write must land in a fresh frame while the poisoned one keeps its
//!    poison;
//! 2. replaces page 0 with a frame of its own filled with a pattern, and does
//!    the same: the child must read the pattern, write into the new frame, and
//!    leave the old frame's poison alone;
//! 3. holds page 0 for a device, decommits and replaces over the hold, and
//!    requires both spaces to still reach the held frame and the child to
//!    still read the marker it wrote;
//! 4. has the child write page 0 and hold still, protects page 0 read-only
//!    in the child's space, and has it write again: the write must fault and
//!    end the child with `SIGSEGV`, not land through the writable entry its
//!    processor cached before the protect.
//!
//! Without the reverse map, step 1's poisoned frame is still mapped and the
//! child reads it. With the reverse map but no shootdown to the child's
//! processor, the child's warm TLB entry reads it all the same.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_frame::Frame;
use ferrix_sched_class::CpuSet;
use ferrix_sched_fair::NICE_0_WEIGHT;
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::mm;
use crate::smp::{self, ScopedShootdowns};
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::{image, registry, uaccess};
use crate::user::vmo::Vmo;

/// Where both processes map the shared object: page 0 under test, page 1 the
/// control page. Written into each architecture's program as a constant.
const BASE: u64 = 0x7000_0000;

/// Bytes of the control page each program's slot takes: command, answer,
/// and two words of what it saw.
const SLOT: usize = 16;

/// The command word: touch page 0, reading it, until told otherwise.
const WARM: u32 = 0;
/// Stop touching page 0, and answer 1.
const HOLD: u32 = 1;
/// Read page 0, write the marker, read it back, report both, answer 2.
const PROBE: u32 = 2;
/// Read page 0 and report it, answer 3.
const PEEK: u32 = 3;
/// Exit with status 0.
const EXIT: u32 = 4;

/// The marker a program writes: this plus its role, 0 for the parent and 1
/// for the child.
const MARK: u32 = 0x5EED_0000;

/// What a frame given back is filled with once claimed.
const POISON: u32 = 0xDEAD_BEEF;

/// What the frame a replace puts in holds.
const PATTERN: u32 = 0x0DDC_0DE5;

/// The status a program exits with when it reads the poison while touching.
const SAW_POISON: i32 = 7;

/// The signal a write to a read-only page ends the child with: 11 on all
/// three architectures.
const SIGSEGV: u32 = 11;

/// The parent's role.
const PARENT: usize = 0;
/// The child's role.
const CHILD: usize = 1;

/// How long a program has to answer a command.
const ANSWER_NANOS: u64 = 10_000_000_000;

/// How long the programs touch page 0 before being told to hold still.
const WARM_NANOS: u64 = 10_000_000;

/// How long the whole check may take on its processor.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// What the check measured, for the boot log.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    /// Frames taken away from two processes and claimed back poisoned.
    pub taken: u32,
    /// The processor the kernel took them on, and the child's.
    pub processors: (usize, usize),
    /// Scoped shootdowns across the measured run.
    pub scoped: ScopedShootdowns,
    /// Whole-TLB shootdowns across the measured run.
    pub global: u64,
    /// Frames the measured run did not give back.
    pub leaked: i64,
}

/// Run it: once to warm the heap up, then settled, counted and measured.
/// `None` on a machine with one processor, or an architecture with no
/// program for it.
pub fn run() -> Result<Option<Report>, &'static str> {
    let online = smp::topology().map_or(1, smp::Topology::online);
    if arch::USER_RMAP_PROGRAM.is_empty() || online < 2 {
        return Ok(None);
    }

    // Warm-up, for the reason stage 7's handler checks give: the kernel heap
    // keeps a page of each size class the first run touches.
    let _ = run_pinned()?;

    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let scoped_before = smp::scoped_shootdowns();
    let global_before = smp::shootdowns();

    let mut report = run_pinned()?;

    report.scoped = ScopedShootdowns {
        sent: report.scoped.sent - scoped_before.sent,
        processors: report.scoped.processors - scoped_before.processors,
        unsent: report.scoped.unsent - scoped_before.unsent,
    };
    report.global -= global_before;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    report.leaked = window.kept();
    if report.leaked != 0 {
        window.report("rmap");
        crate::console::println!(
            "  rmap     {} frames not given back across the check",
            report.leaked
        );
        return Err("taking pages away from two processes did not give back every frame");
    }
    Ok(Some(report))
}

/// Where the pinned task leaves its answer.
static OUTCOME: SpinLock<Option<Result<Report, &'static str>>> = SpinLock::new(None);

/// Run [`check`] in a task pinned to this processor, and wait for it.
///
/// Pinned because "on processor A" has to mean one processor for the whole
/// check, and the task this is called from may be moved between two of its
/// steps.
fn run_pinned() -> Result<Report, &'static str> {
    let here = smp::this_cpu()
        .ok_or("no processor to run the reverse map check on")?
        .logical;
    *OUTCOME.lock() = None;
    let task = crate::sched::spawn_on(
        "rmap-check",
        check_on,
        here,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        // Taken out first, so the lock's guard is gone before anything below
        // sleeps: a spin lock held across the wait for the task is a lock held
        // across a block, which the preemption count refuses.
        let posted = OUTCOME.lock().take();
        if let Some(outcome) = posted {
            // Posting the outcome is the task's last act, and its exit is still
            // ahead of it. Until it has exited the reaper cannot see it, so a
            // caller that counted frames next -- this check's own measured run,
            // after the warm-up -- would have its stack freed inside that count.
            while !task.is_dead() {
                if crate::timer::now_nanos() >= deadline {
                    return Err("the reverse map check's task never exited after it answered");
                }
                crate::sched::sleep_for(1_000_000);
            }
            return outcome;
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("the reverse map check never finished on its processor");
        }
        crate::sched::sleep_for(5_000_000);
    }
}

/// The pinned task's body.
fn check_on(here: usize) {
    let outcome = check(here);
    *OUTCOME.lock() = Some(outcome);
}

/// The whole check, on processor `a`.
///
/// Verifies: L.user.32
fn check(a: usize) -> Result<Report, &'static str> {
    let count = smp::topology().map_or(1, smp::Topology::count);
    let b = (a + 1) % count;

    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_RMAP_PROGRAM,
    );
    let parent = process::load(&file, &[b"/rmap"], &[], [0x5a; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "the reverse map check's program could not be loaded")?;
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let id = parent
        .space()
        .map_anonymous(BASE, 2 * PAGE_SIZE, shared)
        .map_err(|_| "the shared object could not be mapped where the program expects it")?;
    let vmo = parent
        .space()
        .object(id)
        .ok_or("a mapped shared region has no object behind it")?;
    // The control page committed, and both slots told to touch, before
    // either program runs.
    vmo.write_page(1, 0, &[0; 2 * SLOT])
        .map_err(|_| "the control page could not be written")?;

    // The memory half of fork, and a process around it, as `clone` makes one.
    let child_space = parent
        .space()
        .fork()
        .map_err(|_| "the address space holding the shared object could not be forked")?;
    let child = registry::register(
        Process::forked(&parent, child_space, false, false).map_err(|_| "no memory for a fork")?,
    );
    // The child's role is its argument count: two, where the parent has one.
    let stack = child
        .startup()
        .ok_or("a forked process has no program start")?
        .stack;
    uaccess::copy_to_user(child.space(), stack, &2_u32.to_le_bytes())
        .map_err(|_| "the child's argument count could not be written")?;
    if vmo.mapper_count() != 2 {
        return Err("a shared object is not attached to both the address spaces that map it");
    }

    let parent_task = process::start_on(&parent, Some(a))
        .map_err(|_| "the parent of the reverse map check could not be started")?;
    let child_task = process::start_on(&child, Some(b))
        .map_err(|_| "the child of the reverse map check could not be started")?;

    let programs = Programs {
        vmo: &vmo,
        processes: [&parent, &child],
    };
    let outcome = take_pages_away(&programs);

    // Whatever happened, both programs are told to leave, and are ended if
    // they will not, so that nothing outlives the check.
    let _ = programs.command(PARENT, EXIT);
    let _ = programs.command(CHILD, EXIT);
    let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
    let statuses = [
        parent.wait_for_exit(deadline),
        child.wait_for_exit(deadline),
    ];
    for (process, status) in [&parent, &child].into_iter().zip(statuses) {
        if status.is_none() {
            process::kill(process, 137);
        }
    }
    for task in [&parent_task, &child_task] {
        while !task.is_dead() {
            crate::sched::sleep_for(1_000_000);
        }
    }

    // Both address spaces gone before the check returns, so that what they
    // held is given back inside this check's frame window and not a later
    // one's. Both processes are released by now: `wait_for_exit` reports only
    // once a release has come, and a killed one is released when its last
    // thread leaves, before its task is dead. But the release keeps the address
    // space, whose last reference goes when the last task holding it is reaped,
    // in the reaper's `reap_batch`, so this waits on the reaper. The caller's
    // `wait_until_reaper_quiet` is not enough on its own, because it says the
    // reaper is idle, not that these two spaces were among what it reaped.
    let child_signal = child.ended_by_signal();
    let spaces = [
        Arc::downgrade(parent.space()),
        Arc::downgrade(child.space()),
    ];
    drop((parent_task, child_task, parent, child));
    wait_for_spaces_to_go(&spaces)?;

    let taken = outcome?;
    if statuses.contains(&Some(SAW_POISON)) {
        return Err(
            "a process read the poison pattern while touching the shared page: a stale \
             translation outlived the frame it reached",
        );
    }
    // The parent leaves when told; the child was ended by the protect step.
    let [parent_status, _] = statuses;
    if parent_status != Some(0) || child_signal != Some(SIGSEGV) {
        return Err("a process in the reverse map check did not end as the check drove it");
    }
    Ok(Report {
        taken,
        processors: (a, b),
        scoped: smp::scoped_shootdowns(),
        global: smp::shootdowns(),
        leaked: 0,
    })
}

/// Wait until neither of the check's two address spaces has a reference left,
/// for the reason given where [`check`] calls it.
fn wait_for_spaces_to_go(
    spaces: &[alloc::sync::Weak<crate::user::space::AddressSpace>; 2],
) -> Result<(), &'static str> {
    let released_by = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while spaces.iter().any(|space| space.strong_count() > 0) {
        if crate::timer::now_nanos() >= released_by {
            return Err(
                "a process of the reverse map check kept its address space after its task was \
                 reaped",
            );
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// The two programs, and the object through whose control page they are
/// driven.
struct Programs<'a> {
    /// The shared object.
    vmo: &'a Arc<Vmo>,
    /// The parent and the child, by role.
    processes: [&'a Arc<Process>; 2],
}

impl Programs<'_> {
    /// Write word `value` at `offset` into the control page.
    fn put(&self, offset: usize, value: u32) -> Result<(), &'static str> {
        self.vmo
            .write_page(1, offset, &value.to_le_bytes())
            .map_err(|_| "the control page could not be written")
    }

    /// The word at `offset` in the control page.
    fn get(&self, offset: usize) -> Result<u32, &'static str> {
        let mut word = [0; 4];
        self.vmo
            .read_page(1, offset, &mut word)
            .map_err(|_| "the control page could not be read")?;
        Ok(u32::from_le_bytes(word))
    }

    /// Give program `role` a command: its answer cleared first, then the
    /// command, so an answer seen afterwards is to this command.
    fn command(&self, role: usize, command: u32) -> Result<(), &'static str> {
        self.put(role * SLOT + 4, 0)?;
        self.put(role * SLOT, command)
    }

    /// Wait for program `role` to answer `answer`.
    fn answered(&self, role: usize, answer: u32) -> Result<(), &'static str> {
        let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
        loop {
            if self.get(role * SLOT + 4)? == answer {
                return Ok(());
            }
            if let Some(process) = self.processes.get(role)
                && process.is_terminated()
            {
                return Err(if process.exit_status() == Some(SAW_POISON) {
                    "a process read the poison pattern while touching the shared page: a stale \
                     translation outlived the frame it reached"
                } else {
                    "a process in the reverse map check ended before it answered"
                });
            }
            if crate::timer::now_nanos() >= deadline {
                return Err("a process in the reverse map check stopped answering its commands");
            }
            crate::sched::sleep_for(1_000_000);
        }
    }

    /// Have program `role` read page 0, write its marker, and read it back.
    /// What it read first, and what it read back.
    fn probe(&self, role: usize) -> Result<(u32, u32), &'static str> {
        self.command(role, PROBE)?;
        self.answered(role, PROBE)?;
        Ok((self.get(role * SLOT + 8)?, self.get(role * SLOT + 12)?))
    }

    /// Have program `role` read page 0, and say what it read.
    fn peek(&self, role: usize) -> Result<u32, &'static str> {
        self.command(role, PEEK)?;
        self.answered(role, PEEK)?;
        self.get(role * SLOT + 8)
    }

    /// Let both programs touch page 0 for a while, so each processor's TLB
    /// holds its entry, then have both stop touching it.
    fn warm_then_hold(&self) -> Result<(), &'static str> {
        self.command(PARENT, WARM)?;
        self.command(CHILD, WARM)?;
        crate::sched::sleep_for(WARM_NANOS);
        self.command(PARENT, HOLD)?;
        self.command(CHILD, HOLD)?;
        self.answered(PARENT, HOLD)?;
        self.answered(CHILD, HOLD)
    }

    /// Where page 0 translates in each program's space.
    fn translations(&self) -> [Option<u64>; 2] {
        self.processes
            .map(|process| mm::translate_in(process.space().root_table(), BASE))
    }
}

/// The marker the child writes.
const CHILD_MARK: u32 = MARK + CHILD as u32;

/// The marker the parent writes.
const PARENT_MARK: u32 = MARK + PARENT as u32;

/// Steps 1 to 3 of the module's list. How many frames were taken back.
fn take_pages_away(programs: &Programs<'_>) -> Result<u32, &'static str> {
    // Both translate page 0, each on its own processor: the child writes, the
    // parent sees it.
    let (_, written) = programs.probe(CHILD)?;
    if written != CHILD_MARK {
        return Err("a process did not read back the marker it wrote to a shared page");
    }
    let (seen, _) = programs.probe(PARENT)?;
    if seen != CHILD_MARK {
        return Err("a write to a shared page in one process was not seen by the other");
    }

    decommit_under_both(programs)?;
    replace_under_both(programs)?;
    hold_under_both(programs)?;
    protect_under_child(programs)?;
    Ok(2)
}

/// Step 4: a protect reaches the processor that cached the old permission.
///
/// The child has written page 0, so the entry its processor holds is
/// writable; it then holds still, reading only the control page, so nothing
/// replaces that entry. Protected read-only from this processor, the page must
/// refuse the child's next write from user mode, and the refusal ends the
/// child. A protect that took the entry down without telling the child's
/// processor lets the write through, and the child answers.
///
/// Verifies: L.user.88, L.smp.23
fn protect_under_child(programs: &Programs<'_>) -> Result<(), &'static str> {
    let [_, child] = programs.processes;
    let (_, written) = programs.probe(CHILD)?;
    if written != CHILD_MARK {
        return Err("a process did not read back the marker it wrote to a shared page");
    }
    programs.warm_then_hold()?;
    let read_only = VmaFlags {
        shared: true,
        ..VmaFlags::READ
    };
    child
        .space()
        .protect(BASE, PAGE_SIZE, read_only)
        .map_err(|_| "a shared page could not be protected read-only")?;
    if mm::translate_in(child.space().root_table(), BASE).is_some() {
        return Err("a protected page still translates with its old permissions");
    }

    programs.command(CHILD, PROBE)?;
    let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
    while !child.is_terminated() {
        if programs.get(CHILD * SLOT + 4)? == PROBE {
            return Err(
                "a write through a mapping protect had made read-only was let through on the \
                 other processor: its old writable translation outlived the protect",
            );
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("a process writing a page protected read-only neither faulted nor wrote");
        }
        crate::sched::sleep_for(1_000_000);
    }
    if child.ended_by_signal() != Some(SIGSEGV) {
        return Err("a write to a page protected read-only did not end the process with SIGSEGV");
    }
    Ok(())
}

/// Step 1: decommit the page both processes touch.
///
/// Verifies: L.user.16, L.user.90
fn decommit_under_both(programs: &Programs<'_>) -> Result<(), &'static str> {
    let vmo = programs.vmo;
    let child_mark = CHILD_MARK;
    programs.warm_then_hold()?;
    let frame = vmo
        .page(0)
        .ok_or("a shared page both processes touched has no frame")?;
    if programs.translations() != [Some(frame * PAGE_SIZE); 2] {
        return Err("a shared page does not translate to its object's frame in both processes");
    }
    if vmo.decommit_range(0, 1) != 1 {
        return Err("decommitting a shared page both processes map did not take its frame");
    }
    if programs.translations() != [None; 2] {
        return Err("a decommitted page still translates in an address space that maps it");
    }
    poison(frame)?;
    let (seen, written) = programs.probe(CHILD)?;
    if seen == POISON {
        return Err(
            "the process on the other processor read the poison through its user mapping after \
             a decommit: its translation outlived the frame",
        );
    }
    if !poisoned(frame) {
        return Err(
            "a write through a user mapping after a decommit landed in the frame the decommit \
             gave back",
        );
    }
    if seen != 0 || written != child_mark {
        return Err("a decommitted shared page did not come back as a fresh zeroed page");
    }
    if vmo.page(0) == Some(frame) {
        return Err("a decommitted page came back with the frame it gave up");
    }
    let (seen, _) = programs.probe(PARENT)?;
    if seen != child_mark || !poisoned(frame) {
        return Err("the parent did not see the child's write to a page faulted back in");
    }
    let _ = mm::release_frame(frame);
    Ok(())
}

/// Step 2: replace the page both processes touch, with a frame of the
/// kernel's choosing.
///
/// Verifies: L.user.17
fn replace_under_both(programs: &Programs<'_>) -> Result<(), &'static str> {
    let vmo = programs.vmo;
    let (child_mark, parent_mark) = (CHILD_MARK, PARENT_MARK);
    programs.warm_then_hold()?;
    let old = vmo
        .page(0)
        .ok_or("a shared page both processes touched has no frame")?;
    let new = mm::allocate_frames(0).ok_or("no frame to replace a shared page with")?;
    fill(new, PATTERN);
    if vmo.replace(0, new) != Some(old) {
        return Err("replacing a shared page did not report the frame it displaced");
    }
    if programs.translations() != [None; 2] {
        return Err("a replaced page still translates in an address space that maps it");
    }
    poison(old)?;
    let (seen, written) = programs.probe(CHILD)?;
    if seen == POISON {
        return Err(
            "the process on the other processor read the poison through its user mapping after \
             a replace: its translation outlived the frame",
        );
    }
    if !poisoned(old) {
        return Err(
            "a write through a user mapping after a replace landed in the frame the replace \
             gave back",
        );
    }
    if seen != PATTERN || written != child_mark || first_word(new) != child_mark {
        return Err("the process on the other processor did not reach the frame a replace put in");
    }
    let (seen, _) = programs.probe(PARENT)?;
    if seen != child_mark || first_word(new) != parent_mark {
        return Err("the parent did not reach the frame a replace put in");
    }
    let _ = mm::release_frame(old);
    Ok(())
}

/// Step 3: a held page stays, mapped in both processes, whatever is asked of
/// it.
///
/// Verifies: L.user.23
fn hold_under_both(programs: &Programs<'_>) -> Result<(), &'static str> {
    let vmo = programs.vmo;
    let child_mark = CHILD_MARK;
    let held = vmo
        .hold(0, 1)
        .map_err(|_| "a shared page could not be held")?;
    let &[kept] = held.frames() else {
        return Err("a hold of one page did not report one frame");
    };
    let _ = programs.probe(PARENT)?;
    let (_, written) = programs.probe(CHILD)?;
    if written != child_mark || first_word(kept) != child_mark {
        return Err("a write through a user mapping did not reach a held page's frame");
    }
    programs.warm_then_hold()?;
    let reached = [Some(kept * PAGE_SIZE); 2];
    if programs.translations() != reached {
        return Err("a held page is not mapped in both processes");
    }
    if vmo.decommit_range(0, 1) != 0 {
        return Err("a decommit took a held page");
    }
    let spare = mm::allocate_frames(0).ok_or("no frame to try replacing a held page with")?;
    let refused = vmo.replace(0, spare).is_none();
    let _ = mm::release_frame(spare);
    if !refused || vmo.page(0) != Some(kept) {
        return Err("a replace swapped out a held page");
    }
    if programs.translations() != reached {
        return Err("a decommit or replace over a held page took its user mapping away");
    }
    if programs.peek(CHILD)? != child_mark || programs.peek(PARENT)? != child_mark {
        return Err("a process no longer reads the marker written to a held page");
    }
    if programs.translations() != reached {
        return Err("a held page moved while the processes read it");
    }
    drop(held);
    Ok(())
}

/// Claim `frame` back from the allocator the moment it was given up, and fill
/// it with [`POISON`].
fn poison(frame: Frame) -> Result<(), &'static str> {
    let _ = mm::claim_frame(frame).ok_or(
        "the frame a shared page gave up was taken by something else before it could be poisoned",
    )?;
    fill(frame, POISON);
    Ok(())
}

/// Fill `frame` with `word`, over and over.
fn fill(frame: Frame, word: u32) {
    let base = mm::direct_map(frame * PAGE_SIZE) as *mut u32;
    for index in 0..(PAGE_SIZE / 4) as usize {
        // SAFETY: (FRAME) `index` words is inside the page `base` starts, which the
        // direct map covers.
        let at = unsafe { base.add(index) };
        // SAFETY: (FRAME) the check allocated or claimed `frame`, so nothing else owns
        // it, and `at` is a word inside it.
        unsafe { at.write_volatile(word) };
    }
}

/// Whether every word of `frame` is still [`POISON`].
fn poisoned(frame: Frame) -> bool {
    let base = mm::direct_map(frame * PAGE_SIZE) as *const u32;
    (0..(PAGE_SIZE / 4) as usize).all(|index| {
        // SAFETY: (FRAME) as `fill`: inside the page `base` starts.
        let at = unsafe { base.add(index) };
        // SAFETY: (FRAME) as `fill`; the frame is still the check's.
        unsafe { at.read_volatile() == POISON }
    })
}

/// The first word of `frame`.
fn first_word(frame: Frame) -> u32 {
    // SAFETY: (FRAME) `frame` is allocated -- the object or the check holds it -- and
    // the direct map covers every frame of RAM.
    unsafe { (mm::direct_map(frame * PAGE_SIZE) as *const u32).read_volatile() }
}

/// The ELF class a program for this build is, for the reason stage 7's
/// checks give.
fn class_of_this_build() -> Class {
    if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    }
}
