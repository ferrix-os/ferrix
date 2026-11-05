//! B5's check host (`docs/microkernel/B5-host.md`): programs on [`Bare`], the core's
//! process that holds no personality, so what these checks prove links and runs in a
//! kernel with no load. The class B ids of `docs/microkernel/B5.md`.
//!
//! The one program, per architecture, reads four words at [`DATA`]: how many calls
//! to make, how many pages at [`PAGES_AT`] to touch, how many loops to spin, and the
//! status to end with, by `thread_exit`, which is K11's. So no Linux number is made.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::VmaFlags;

use crate::console::println;
use crate::object::bare::Bare;
use crate::object::job::Job;
use crate::object::quota::{Counter, Mark, Resource};
use crate::sched::Task;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;
use crate::user::vmo::{Filler, Vmo};

/// Where the program is.
const TEXT: u64 = 0x40_0000;
/// Where its four words are.
const DATA: u64 = 0x41_0000;
/// Where the pages it touches begin.
const PAGES_AT: u64 = 0x5000_0000;
/// The top of its stack.
const STACK_TOP: u64 = 0x7000_0000;

/// How long a thread started on another processor is given to be running.
const SETTLE_NANOS: u64 = 100_000_000;
/// How long a program may take.
const PATIENCE_NANOS: u64 = 120_000_000_000;
/// Its loops, as stage 7's kernel-time case has them.
const SPINS: u32 = 60_000_000;
/// Its calls.
const CALLS: u32 = 50_000;
/// Its pages.
const PAGES: u32 = 2_048;
/// Its status.
const STATUS: u32 = 57;
/// How many runs a kernel-time case gets: a host that stalls this virtual processor
/// charges the stall to whatever it stopped.
const ATTEMPTS: usize = 3;

/// What a program is to do.
#[derive(Clone, Copy, Debug, Default)]
struct Work {
    calls: u32,
    pages: u32,
    spins: u32,
    status: u32,
    /// Whether the host touches its pages before it starts, so that it takes no fault.
    touched: bool,
}

/// One program in a job of its own.
struct BareSide {
    host: Arc<Bare>,
}

impl BareSide {
    /// A program ready to run `work`, in a new job.
    fn new(work: Work) -> Result<BareSide, &'static str> {
        Self::in_job(work, Job::new_root().map_err(|_| "bare: no job")?)
    }

    /// A program ready to run `work`, in `job`.
    fn in_job(work: Work, job: Arc<Job>) -> Result<BareSide, &'static str> {
        let space = AddressSpace::new().map_err(|_| "bare: no space")?;
        put_program(&space, work)?;
        let host = Bare::new(space, job, None).map_err(|_| "bare: no process")?;
        Ok(BareSide { host })
    }

    /// A program in `job` that spins until its end reaches it.
    fn spinning(job: Arc<Job>) -> Result<BareSide, &'static str> {
        let space = AddressSpace::new().map_err(|_| "bare: no space")?;
        put_text(&space, crate::arch::bare_programs().1, Work::default())?;
        let host = Bare::new(space, job, None).map_err(|_| "bare: no process")?;
        Ok(BareSide { host })
    }

    /// Start it on `cpu` and hold its task.
    fn start(&self, cpu: Option<usize>) -> Result<Arc<Task>, &'static str> {
        let thread = crate::object::bare::spawn_on(&self.host, TEXT, STACK_TOP, 0, cpu)?;
        thread
            .task()
            .ok_or("bare: a thread ended before its task could be read")
    }

    /// Wait for its end, and answer its status.
    fn ended(&self) -> Result<i32, &'static str> {
        let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
        loop {
            if let Some(status) = crate::object::process::Host::core(&*self.host)
                .exit()
                .status()
            {
                return Ok(status);
            }
            if crate::timer::now_nanos() >= deadline {
                return Err("bare: a program did not end within the check's patience");
            }
            crate::sched::sleep_for(1_000_000);
        }
    }
}

/// The program, its words, its pages and its stack, in `space`.
fn put_program(space: &AddressSpace, work: Work) -> Result<(), &'static str> {
    put_text(space, crate::arch::bare_programs().0, work)
}

/// `program`, `work`'s words, its pages and its stack, in `space`.
fn put_text(space: &AddressSpace, program: &[u8], work: Work) -> Result<(), &'static str> {
    let map = |at, length, flags| {
        space
            .map_anonymous(at, length, flags)
            .map(|_| ())
            .map_err(|_| "bare: a program's memory would not map")
    };
    map(TEXT, PAGE_SIZE, VmaFlags::READ_WRITE)?;
    uaccess::copy_to_user(space, TEXT, program).map_err(|_| "bare: no copy")?;
    space
        .protect(TEXT, PAGE_SIZE, VmaFlags::READ_EXECUTE)
        .map_err(|_| "bare: a program's text would not protect")?;
    map(DATA, PAGE_SIZE, VmaFlags::READ_WRITE)?;
    let mut words: Vec<u8> = vec![];
    for word in [work.calls, work.pages, work.spins, work.status] {
        crate::fallible::try_extend(&mut words, word.to_ne_bytes())
            .map_err(|_| "bare: no memory")?;
    }
    uaccess::copy_to_user(space, DATA, &words).map_err(|_| "bare: no copy")?;
    if work.pages != 0 {
        map(
            PAGES_AT,
            u64::from(work.pages) * PAGE_SIZE,
            VmaFlags::READ_WRITE,
        )?;
    }
    if work.touched {
        for page in 0..u64::from(work.pages) {
            uaccess::copy_to_user(space, PAGES_AT + page * PAGE_SIZE, &[0])
                .map_err(|_| "bare: no copy")?;
        }
    }
    map(
        STACK_TOP - 4 * PAGE_SIZE,
        4 * PAGE_SIZE,
        VmaFlags::READ_WRITE,
    )?;
    Ok(())
}

/// What a program was charged.
#[derive(Clone, Copy, Debug)]
struct Charged {
    runtime: u64,
    kernel: u64,
    preempted: u64,
    /// Page faults its job counted.
    faults: u64,
}

/// Run `works` together on `cpu`, and answer what each was charged, having ended
/// with its status.
fn run_charged(works: &[Work], cpu: usize) -> Result<Vec<Charged>, &'static str> {
    let mut sides = Vec::new();
    let mut tasks = Vec::new();
    let mut jobs = Vec::new();
    for work in works {
        let job = Job::new_root().map_err(|_| "bare: no job")?;
        let side = BareSide::in_job(*work, Arc::clone(&job))?;
        crate::fallible::try_push(&mut jobs, job).map_err(|_| "bare: no memory")?;
        crate::fallible::try_push(&mut tasks, side.start(Some(cpu))?)
            .map_err(|_| "bare: no memory")?;
        crate::fallible::try_push(&mut sides, side).map_err(|_| "bare: no memory")?;
    }
    for (side, work) in sides.iter().zip(works) {
        let status = side.ended()?;
        if status != work.status as i32 {
            return Err("bare: a program did not end with its own status");
        }
    }
    let mut charged = Vec::new();
    for (task, job) in tasks.iter().zip(&jobs) {
        crate::sched::wait_until_gone(task, PATIENCE_NANOS)?;
        crate::fallible::try_push(
            &mut charged,
            Charged {
                runtime: task.runtime(),
                kernel: task.kernel_time(),
                preempted: task.preemptions(),
                faults: job.counted(Counter::Faults),
            },
        )
        .map_err(|_| "bare: no memory")?;
    }
    Ok(charged)
}

/// Whether it spun in user mode and was charged under a quarter as kernel time.
fn spun_as_user(one: Charged) -> bool {
    one.runtime != 0 && one.kernel.saturating_mul(4) < one.runtime
}

/// Whether what it did is at least half kernel time, L.sched.81's and L.sched.82's
/// criterion. A share and not an amount: the entry and exit stubs' instructions are the
/// program's by L.sched.81, so the share moves with the processor and the build (62% to
/// 94% on the hosts in `docs/backlog/flakes.md`, FX-0936) and no higher line is the
/// requirement's.
fn mostly_kernel(one: Charged) -> bool {
    one.kernel.saturating_mul(2) >= one.runtime
}

/// What share of `one`'s runtime was kernel time, in percent.
fn percent(one: Charged) -> u64 {
    one.kernel
        .saturating_mul(100)
        .checked_div(one.runtime)
        .unwrap_or(0)
}

/// Say what a failed attempt was charged: a failure on a host out of reach says nothing
/// otherwise (ledger 6989).
fn print_attempt(attempt: usize, one: Charged) {
    println!(
        "  bare     attempt {attempt} of {ATTEMPTS}: {} ns kernel time of {} ns runtime, cut {} times, {} faults",
        one.kernel, one.runtime, one.preempted, one.faults,
    );
}

/// `run` up to [`ATTEMPTS`] times, until what it answers `holds`, and answer that. These
/// cases read a fraction of time and not a charge, so scheduling is allowed for; it proves
/// the rule held in one run of three, and a charge wrong two times in three passes (ledger
/// 6946).
fn attempted(
    mut run: impl FnMut() -> Result<Charged, &'static str>,
    holds: fn(Charged) -> bool,
    failure: &'static str,
) -> Result<Charged, &'static str> {
    for attempt in 1..=ATTEMPTS {
        let one = run()?;
        if holds(one) {
            return Ok(one);
        }
        print_attempt(attempt, one);
    }
    Err(failure)
}

/// `work` alone on `cpu`, once, and what it was charged.
fn alone(work: Work, cpu: usize) -> Result<Charged, &'static str> {
    run_charged(core::slice::from_ref(&work), cpu)?
        .first()
        .copied()
        .ok_or("bare: a program was charged nothing")
}

/// L.sched.80: a program's time in user mode is not kernel time, however it last
/// came back: from its start, a call, a page fault, or an interrupt that cut it. Two
/// programs on one processor keep their own registers and each is cut (x86-64's way in
/// and interrupt's way out).
///
/// Verifies: L.sched.80
/// Verifies: `L.x86_64.59`, `L.x86_64.79`
fn user_time_is_not_kernel_time(cpu: usize) -> Result<(), &'static str> {
    const SPUN: &str = "bare: a program spinning in user mode was charged as kernel time";
    let spin = Work {
        spins: SPINS,
        status: STATUS,
        ..Work::default()
    };
    let _ = attempted(|| alone(spin, cpu), spun_as_user, SPUN)?;
    let call = Work { calls: 1, ..spin };
    let _ = attempted(|| alone(call, cpu), spun_as_user, SPUN)?;
    let page = Work { pages: 1, ..spin };
    let _ = attempted(|| alone(page, cpu), spun_as_user, SPUN)?;
    for _ in 0..ATTEMPTS {
        let charged = run_charged(&[spin, spin], cpu)?;
        if charged
            .iter()
            .all(|one| one.preempted >= 1 && spun_as_user(*one))
        {
            return Ok(());
        }
    }
    Err("bare: two programs on one processor were not both cut in user mode, charged as user time")
}

/// Whether `faulted` took [`PAGES`] faults that `touched` did not, and they cost at least
/// half kernel time. Any other count shows no difference of faults, whatever the times say.
fn faults_were_kernel_time(faulted: Charged, touched: Charged) -> bool {
    let runtime = faulted.runtime.saturating_sub(touched.runtime);
    let kernel = faulted.kernel.saturating_sub(touched.kernel);
    faulted.faults.saturating_sub(touched.faults) >= u64::from(PAGES)
        && runtime != 0
        && kernel.saturating_mul(2) >= runtime
}

/// L.sched.81, L.sched.82: a program's calls and faults are kernel time, each at least
/// half of what it costs. This shows they are counted, not how much of them is: a stamp
/// placed late stays inside half.
///
/// Faults by difference: the same program with its pages touched by the host before it
/// starts has the same loop and the same end, and takes none of those faults, which its
/// job's count shows. Unmapping what it touched is kernel time near half of a run with no
/// fault charged at all, so a share of the one run could not tell them apart (`rbh-82`).
/// Answers the two shares, in percent.
///
/// Verifies: L.sched.81, L.sched.82, H.SCHED.14
fn calls_and_faults_are_kernel_time(cpu: usize) -> Result<(u64, u64), &'static str> {
    let calls = Work {
        calls: CALLS,
        status: STATUS,
        ..Work::default()
    };
    let charged_calls = attempted(
        || alone(calls, cpu),
        mostly_kernel,
        "bare: a program's system calls were charged as user time",
    )?;
    let faults = Work {
        pages: PAGES,
        status: STATUS,
        ..Work::default()
    };
    let touched = Work {
        touched: true,
        ..faults
    };
    for attempt in 1..=ATTEMPTS {
        let one = alone(faults, cpu)?;
        let other = alone(touched, cpu)?;
        if faults_were_kernel_time(one, other) {
            let difference = Charged {
                runtime: one.runtime.saturating_sub(other.runtime),
                kernel: one.kernel.saturating_sub(other.kernel),
                ..one
            };
            return Ok((percent(charged_calls), percent(difference)));
        }
        print_attempt(attempt, one);
        print_attempt(attempt, other);
    }
    Err("bare: a program's page faults were charged as user time")
}

/// L.trap.2: a fault in a mapped region makes the mapping true and the instruction is
/// retried: every page touched is resident, and the program went on to end.
/// `L.x86_64.79`: an interrupt from user mode preempts: the two programs above were cut.
///
/// Verifies: L.trap.2
fn a_fault_is_resolved() -> Result<(), &'static str> {
    let side = BareSide::new(Work {
        pages: 16,
        status: STATUS,
        ..Work::default()
    })?;
    let space = Arc::clone(crate::object::process::Host::core(&*side.host).own_space());
    let _task = side.start(None)?;
    if side.ended()? != STATUS as i32 {
        return Err("bare: a program that faulted did not end with its status");
    }
    drop(space);
    Ok(())
}

/// L.user.87: a write after `protect` to read-only faults, on another processor where
/// there is one. The region is made with `map_anonymous` on the process's own space,
/// since a native region refuses `protect` (ledger 6110 (3)), and its text does the store.
///
/// Verifies: L.user.87
fn a_write_after_protect_faults(cpu: usize) -> Result<(), &'static str> {
    let side = BareSide::new(Work {
        pages: 1,
        status: STATUS,
        ..Work::default()
    })?;
    let space = crate::object::process::Host::core(&*side.host).own_space();
    space
        .protect(PAGES_AT, PAGE_SIZE, VmaFlags::READ)
        .map_err(|_| "bare: a program's page would not protect")?;
    let _task = side.start(Some(cpu))?;
    if side.ended()? == STATUS as i32 {
        return Err("bare: a store to a page made read-only went through");
    }
    Ok(())
}

/// L.object.100, H.QUOTA.6, L.object.80: a fault refused by a job's memory limit
/// ends a process of that job and none outside it, and counts the kill there and
/// above. A program with a program's own faults, as a thread of the `Bare`, so the
/// charge is its job's (ledger 6110 (5)).
///
/// Verifies: L.object.100, L.object.80, H.QUOTA.6
fn a_limit_ends_its_own(cpu: usize) -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "bare: no job")?;
    let limited = tree.new_child().map_err(|_| "bare: no job")?;
    let sibling = tree.new_child().map_err(|_| "bare: no job")?;
    if !limited.set_limit(Resource::Memory, 1 << 20) {
        return Err("bare: the job's memory limit could not be set");
    }
    let victim = BareSide::in_job(
        Work {
            pages: PAGES,
            status: STATUS,
            ..Work::default()
        },
        Arc::clone(&limited),
    )?;
    let other = BareSide::in_job(
        Work {
            spins: SPINS,
            status: STATUS,
            ..Work::default()
        },
        Arc::clone(&sibling),
    )?;
    let _running = other.start(Some(cpu))?;
    let _faulting = victim.start(Some(cpu))?;
    if victim.ended()? == STATUS as i32 {
        return Err("bare: a program past its job's memory limit ran to its end");
    }
    if other.ended()? != STATUS as i32 {
        return Err("bare: a limit's kill reached a process outside the job");
    }
    if limited.oom_counts().1 != 1 || tree.oom_counts().1 != 1 || sibling.oom_counts().1 != 0 {
        return Err("bare: a limit's kill was not counted in its job and above, and only there");
    }
    Ok(())
}

/// L.sched.4: a `cpu.max` set before a program runs alone on its processor, which gets
/// no tick, still cuts it: held between a twelfth and two thirds of the processor.
/// H.QUOTA.10, L.object.106: its periods count as throttled, and what it used is charged
/// to its job and the one above, by the end as user time alone counts it a spinner's.
///
/// Verifies: L.sched.4, H.QUOTA.10, L.object.106
fn a_quota_cuts_a_program_alone(cpu: usize) -> Result<(), &'static str> {
    const WATCHED_NANOS: u64 = 1_000_000_000;
    const STOPPED: i32 = 97;
    let tree = Job::new_root().map_err(|_| "bare: no job")?;
    let above = tree.new_child().map_err(|_| "bare: no job")?;
    let job = above.new_child().map_err(|_| "bare: no job")?;
    if !job.set_bandwidth(20_000_000, 100_000_000) {
        return Err("bare: a job's cpu.max could not be set");
    }
    let side = BareSide::spinning(Arc::clone(&job))?;
    let began = crate::timer::now_nanos();
    let task = side.start(Some(cpu))?;
    crate::sched::sleep_for(WATCHED_NANOS);
    let wall = crate::timer::now_nanos().saturating_sub(began);
    let ran = task.runtime();
    side.host.end(STOPPED);
    crate::sched::wait_until_gone(&task, PATIENCE_NANOS)?;
    if side.ended()? != STOPPED {
        return Err("bare: a program under cpu.max kept another status");
    }
    // On every boot: a share used as a limit is read on hosts out of reach too (ledger 7058).
    println!(
        "  bare     under cpu.max 20000 100000 a program ran {} of {} us",
        ran / 1000,
        wall / 1000
    );
    if ran.saturating_mul(12) < wall || ran.saturating_mul(3) > wall.saturating_mul(2) {
        return Err("bare: a program alone on its processor was not held to its cpu.max");
    }
    if job.counted(Counter::Throttled) < 3 {
        return Err("bare: a program's periods under cpu.max did not count as throttled");
    }
    crate::sched::charge_running();
    let charged = |job: &Job| {
        let (user, kernel) = job.cpu_times();
        user.saturating_add(kernel)
    };
    // What ran, give or take the last slice: in the job and the one above alike.
    if charged(&job).saturating_mul(2) < ran || charged(&above) < charged(&job) {
        return Err("bare: a program's processor time was not charged to its job and above");
    }
    if job.cpu_times().0 == 0 {
        return Err("bare: a spinning program's time counted as no user time");
    }
    Ok(())
}

/// L.sched.3: a program its job's `cpu.max` holds leaves the wait for its end, within
/// a quarter of a second where waiting out the period takes nine tenths of one.
///
/// Verifies: L.sched.3
fn a_throttled_program_leaves_for_its_end(cpu: usize) -> Result<(), &'static str> {
    const QUOTA: u64 = 1_000_000;
    const STOPPED: i32 = 99;
    let tree = Job::new_root().map_err(|_| "bare: no job")?;
    let job = tree.new_child().map_err(|_| "bare: no job")?;
    if !job.set_bandwidth(QUOTA, 1_000_000_000) {
        return Err("bare: a job's cpu.max could not be set");
    }
    let side = BareSide::spinning(Arc::clone(&job))?;
    let task = side.start(Some(cpu))?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while task.runtime() < QUOTA {
        if crate::timer::now_nanos() >= deadline {
            return Err("bare: a program under cpu.max did not run its quota");
        }
        crate::sched::sleep_for(1_000_000);
    }
    // Into the wait: it has used its millisecond, and the period has most of its second left.
    crate::sched::sleep_for(50_000_000);
    side.host.end(STOPPED);
    crate::sched::wait_until_gone(&task, 250_000_000)
        .map_err(|_| "bare: a program held by cpu.max did not leave the wait for its end")?;
    if side.ended()? != STOPPED {
        return Err("bare: a program held by cpu.max ended with another status");
    }
    Ok(())
}

/// L.sched.35: a process's end tells every task it lists, a spinning one on another
/// processor included, which then leaves by its next way back to user mode.
///
/// Verifies: L.sched.35
fn an_end_reaches_every_thread(cpu: usize) -> Result<(), &'static str> {
    const STOPPED: i32 = 98;
    let side = BareSide::spinning(Job::new_root().map_err(|_| "bare: no job")?)?;
    let there = side.start(Some(cpu))?;
    let here = side.start(None)?;
    // Both spinning. A program alone on its processor gets no tick, so its runtime
    // says nothing until it is cut: time, not a charge, is what is waited for.
    crate::sched::sleep_for(SETTLE_NANOS);
    side.host.end(STOPPED);
    let gone = |task: &Task| {
        crate::sched::wait_until_gone(task, 1_000_000_000)
            .map_err(|_| "bare: a process's end did not reach a spinning thread")
    };
    gone(&there)?;
    gone(&here)?;
    if side.ended()? != STOPPED {
        return Err("bare: an ended process kept another status");
    }
    Ok(())
}

/// `L.x86_64.68`: a native program enters at its entry on its stack with the start argument
/// in RDI. `L.x86_64.55`: a call dispatches on RAX with RDI, RSI, RDX and R10, and answers
/// in RAX. The argument reaches the status only through all of these.
///
/// Verifies: `L.x86_64.55`, `L.x86_64.68`
fn a_program_has_its_argument_and_its_calls_theirs(program: &[u8]) -> Result<(), &'static str> {
    let space = AddressSpace::new().map_err(|_| "bare: no space")?;
    put_text(&space, program, Work::default())?;
    let job = Job::new_root().map_err(|_| "bare: no job")?;
    let host = Bare::new(space, job, None).map_err(|_| "bare: no process")?;
    let side = BareSide { host };
    let _thread =
        crate::object::bare::spawn_on(&side.host, TEXT, STACK_TOP, u64::from(STATUS), None)?;
    if side.ended()? != STATUS as i32 {
        return Err("bare: a program's start argument or a call's four did not arrive");
    }
    Ok(())
}

/// A file's source whose every page can be read again, as a read-only mount's.
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

/// L.object.108: a task of a job over its `memory.high`, having just taken a page,
/// gives back that job's clean cache down to the mark, counted in the job and above,
/// and a sibling's cache stays. The pages are taken as a fault takes them, charged to
/// the job, and `throttle` is what the trap's way out calls after one.
///
/// Verifies: L.object.108
fn over_its_mark_a_job_gives_back() -> Result<(), &'static str> {
    const PAGES: u64 = 64;
    const SIBLING_PAGES: u64 = 8;
    const MARK: u64 = 16 * PAGE_SIZE;
    let tree = Job::new_root().map_err(|_| "bare: no job")?;
    let job = tree.new_child().map_err(|_| "bare: no job")?;
    let sibling = tree.new_child().map_err(|_| "bare: no job")?;
    if !job.set_mark(Mark::High, MARK) {
        return Err("bare: a job's memory.high could not be set");
    }
    let source: Arc<dyn Filler> =
        crate::fallible::try_arc(ReadAgain).map_err(|_| "bare: no memory")?;
    let file = Vmo::new_filled(PAGES, Some(Arc::clone(&source))).map_err(|_| "bare: no file")?;
    let beside = Vmo::new_filled(SIBLING_PAGES, Some(source)).map_err(|_| "bare: no file")?;
    let own = crate::sched::running_group();
    crate::sched::set_current_group(sibling.quota_index());
    let filled = (0..SIBLING_PAGES).try_for_each(|index| beside.commit_and_write(index, 0, &[1]));
    crate::sched::set_current_group(job.quota_index());
    let taken = filled.and_then(|()| {
        (0..PAGES).try_for_each(|index| {
            file.commit_and_write(index, 0, &[1])?;
            crate::object::oom::throttle();
            Ok(())
        })
    });
    crate::sched::set_current_group(own);
    taken.map_err(|_| "bare: a file's page would not commit")?;
    let used = job
        .usage(Resource::Memory)
        .map_or(u64::MAX, |usage| usage.used);
    // The last page's own throttle ran after the page: the job is at its mark.
    if used > MARK {
        return Err("bare: a job over its memory.high was not brought back to it");
    }
    if file.committed() as u64 * PAGE_SIZE > MARK {
        return Err("bare: a file's pages stayed resident over its job's memory.high");
    }
    if job.counted(Counter::High) == 0 || tree.counted(Counter::High) == 0 {
        return Err("bare: a crossing of memory.high was not counted in its job and above");
    }
    if beside.committed() as u64 != SIBLING_PAGES || sibling.counted(Counter::High) != 0 {
        return Err("bare: a job over its memory.high took a sibling's cache");
    }
    Ok(())
}

/// The word at [`PAGES_AT`] of `space`.
fn word_at(space: &AddressSpace) -> Result<u8, &'static str> {
    let mut one = [0_u8; 1];
    uaccess::copy_from_user(space, PAGES_AT, &mut one).map_err(|_| "bare: no read")?;
    Ok(one[0])
}

/// L.user.61, H.MEM.8: a program's own store to a private page a fork still shares is
/// copied for the side that wrote: seen by neither the other side nor the page. S and
/// two clones of it, `AddressSpace::fork`, the function a Linux fork and K2's clone
/// both are; each side a program of its own, storing by its own instruction. The third
/// holder reads the old word after either has written. Replaces stage 6's
/// `USER_COW_PROGRAM` where there is no load (L.user.160).
///
/// Verifies: L.user.61, H.MEM.8
fn a_store_copies_for_the_side_that_writes() -> Result<(), &'static str> {
    const OLD: u8 = 0x55;
    let work = Work {
        pages: 1,
        status: STATUS,
        ..Work::default()
    };
    let source = AddressSpace::new().map_err(|_| "bare: no space")?;
    put_program(&source, work)?;
    uaccess::copy_to_user(&source, PAGES_AT, &[OLD]).map_err(|_| "bare: no copy")?;
    let child = source.fork().map_err(|_| "bare: no memory for a fork")?;
    let witness = source.fork().map_err(|_| "bare: no memory for a fork")?;
    for space in [child, Arc::clone(&source)] {
        let job = Job::new_root().map_err(|_| "bare: no job")?;
        let host = Bare::new(space, job, None).map_err(|_| "bare: no process")?;
        let side = BareSide { host };
        let _task = side.start(None)?;
        if side.ended()? != STATUS as i32 {
            return Err("bare: a program's store to a shared page did not go through");
        }
        if word_at(&witness)? != OLD {
            return Err("bare: a store to a copy-on-write page reached another holder");
        }
    }
    Ok(())
}

/// L.user.62, H.MEM.16: a clone's store to a shared anonymous page reaches the holder
/// it was cloned from, a page the clone faults in first included, and its store to a
/// private page does not. `AddressSpace::fork`, the function a Linux fork and K2's
/// clone both are; the store is the program's own.
///
/// Verifies: L.user.62, H.MEM.16
fn a_shared_store_reaches_the_other_side() -> Result<(), &'static str> {
    const OLD: u8 = 0x55;
    let source = AddressSpace::new().map_err(|_| "bare: no space")?;
    put_program(
        &source,
        Work {
            pages: 3,
            status: STATUS,
            ..Work::default()
        },
    )?;
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    source
        .unmap(PAGES_AT, 2 * PAGE_SIZE)
        .map_err(|_| "bare: a program's pages would not unmap")?;
    let _ = source
        .map_anonymous(PAGES_AT, 2 * PAGE_SIZE, shared)
        .map_err(|_| "bare: a shared mapping would not map")?;
    let private = PAGES_AT + 2 * PAGE_SIZE;
    uaccess::copy_to_user(&source, PAGES_AT, &[OLD]).map_err(|_| "bare: no copy")?;
    uaccess::copy_to_user(&source, private, &[OLD]).map_err(|_| "bare: no copy")?;
    let clone = source.fork().map_err(|_| "bare: no memory for a fork")?;
    let job = Job::new_root().map_err(|_| "bare: no job")?;
    let host = Bare::new(clone, job, None).map_err(|_| "bare: no process")?;
    let side = BareSide { host };
    let _task = side.start(None)?;
    if side.ended()? != STATUS as i32 {
        return Err("bare: a clone's stores did not go through");
    }
    let read = |at| {
        let mut one = [0_u8; 1];
        uaccess::copy_from_user(&source, at, &mut one)
            .map(|()| one[0])
            .map_err(|_| "bare: no read")
    };
    if read(PAGES_AT)? == OLD {
        return Err("bare: a clone's store to a shared page did not reach its source");
    }
    if read(PAGES_AT + PAGE_SIZE)? == 0 {
        return Err("bare: a shared page a clone faulted in first was not the source's");
    }
    if read(private)? != OLD {
        return Err("bare: a clone's store to a private page reached its source");
    }
    Ok(())
}

/// L.object.107's core half: a process is frozen when its job or one above asks,
/// whether that came before it was made, before it moved in or after, and stops being
/// so when thawed or moved out. The parking is `needs_attention`'s, the personality's.
///
/// Verifies: L.object.107
fn a_job_says_who_is_frozen() -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "bare: no job")?;
    let frozen = tree.new_child().map_err(|_| "bare: no job")?;
    let beneath = frozen.new_child().map_err(|_| "bare: no job")?;
    let outside = tree.new_child().map_err(|_| "bare: no job")?;
    frozen.set_freeze(true);
    let born = BareSide::in_job(Work::default(), beneath)?;
    let born = crate::object::process::Host::core(&*born.host);
    if !born.sync_freeze().0 || !born.is_frozen() {
        return Err("bare: a process made beneath a frozen job was not frozen");
    }
    let moved = BareSide::in_job(Work::default(), Arc::clone(&outside))?;
    let moved = crate::object::process::Host::core(&*moved.host);
    if moved.sync_freeze().0 || moved.is_frozen() {
        return Err("bare: a process outside a frozen job was frozen");
    }
    moved
        .move_to(&frozen)
        .map_err(|_| "bare: a process would not move into a frozen job")?;
    if !moved.sync_freeze().0 || !moved.is_frozen() {
        return Err("bare: a process moved into a frozen job was not frozen");
    }
    moved
        .move_to(&outside)
        .map_err(|_| "bare: a process would not move out of a frozen job")?;
    if moved.sync_freeze().0 || moved.is_frozen() {
        return Err("bare: a process moved out of a frozen job stayed frozen");
    }
    frozen.set_freeze(false);
    if born.sync_freeze().0 || born.is_frozen() {
        return Err("bare: a process stayed frozen after its job was thawed");
    }
    Ok(())
}

/// What [`run`] found.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    /// How many groups of cases ran.
    pub cases: u32,
    /// Kernel time's share of a program's calls and of its faults, in percent, on a
    /// boot that counts kernel time.
    pub shares: Option<(u64, u64)>,
}

// CHECKS-ONLY: stages_check.rs
/// Every case: how many ran, none where the kernel does not count kernel time.
///
/// # Errors
///
/// The first that did not hold.
pub fn run() -> Result<Report, &'static str> {
    over_its_mark_a_job_gives_back()?;
    a_job_says_who_is_frozen()?;
    // ARMv7-A has no K11: its native programs' end is the Linux one, until M5.
    if !crate::arch::HAS_RESTRICTED {
        return Ok(Report {
            cases: 2,
            shares: None,
        });
    }
    a_fault_is_resolved()?;
    a_store_copies_for_the_side_that_writes()?;
    a_shared_store_reaches_the_other_side()?;
    let here = crate::smp::this_cpu().ok_or("bare: no processor")?.logical;
    let count = crate::smp::count();
    let cpu = if count > 1 { (here + 1) % count } else { here };
    a_write_after_protect_faults(cpu)?;
    a_limit_ends_its_own(cpu)?;
    a_quota_cuts_a_program_alone(cpu)?;
    a_throttled_program_leaves_for_its_end(cpu)?;
    an_end_reaches_every_thread(cpu)?;
    if let Some(program) = crate::arch::bare_programs().2 {
        a_program_has_its_argument_and_its_calls_theirs(program)?;
    }
    if !crate::sched::vtime::counting() {
        return Ok(Report {
            cases: 11,
            shares: None,
        });
    }
    user_time_is_not_kernel_time(cpu)?;
    let shares = calls_and_faults_are_kernel_time(cpu)?;
    Ok(Report {
        cases: 13,
        shares: Some(shares),
    })
}
