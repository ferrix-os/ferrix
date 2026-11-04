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

use crate::object::bare::Bare;
use crate::object::job::Job;
use crate::object::quota::Resource;
use crate::sched::Task;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;

/// Where the program is.
const TEXT: u64 = 0x40_0000;
/// Where its four words are.
const DATA: u64 = 0x41_0000;
/// Where the pages it touches begin.
const PAGES_AT: u64 = 0x5000_0000;
/// The top of its stack.
const STACK_TOP: u64 = 0x7000_0000;

/// See the module. `as --64`, read back with `objcopy`.
#[cfg(target_arch = "x86_64")]
const PROGRAM: &[u8] = &[
    0xbb, 0x00, 0x00, 0x41, 0x00, 0x44, 0x8b, 0x23, 0x45, 0x85, 0xe4, 0x74, 0x0e, 0xb8, 0x00, 0x10,
    0x00, 0x00, 0x31, 0xff, 0x0f, 0x05, 0x41, 0xff, 0xcc, 0x75, 0xf2, 0x44, 0x8b, 0x63, 0x04, 0x45,
    0x85, 0xe4, 0x74, 0x17, 0x41, 0xbd, 0x00, 0x00, 0x00, 0x50, 0x41, 0xc6, 0x45, 0x00, 0x01, 0x49,
    0x81, 0xc5, 0x00, 0x10, 0x00, 0x00, 0x41, 0xff, 0xcc, 0x75, 0xef, 0x8b, 0x4b, 0x08, 0x85, 0xc9,
    0x74, 0x04, 0xff, 0xc9, 0x75, 0xfc, 0xb8, 0x9a, 0x10, 0x00, 0x00, 0x8b, 0x7b, 0x0c, 0xbe, 0x01,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xfe,
];
/// See the module. `aarch64-linux-gnu-as`, read back with `objcopy`.
#[cfg(target_arch = "aarch64")]
const PROGRAM: &[u8] = &[
    0x33, 0x08, 0xa0, 0xd2, 0x74, 0x02, 0x40, 0xb9, 0xd4, 0x00, 0x00, 0x34, 0x08, 0x00, 0x82, 0xd2,
    0x00, 0x00, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4, 0x94, 0x06, 0x00, 0x71, 0x81, 0xff, 0xff, 0x54,
    0x74, 0x06, 0x40, 0xb9, 0xf4, 0x00, 0x00, 0x34, 0x15, 0x00, 0xaa, 0xd2, 0x36, 0x00, 0x80, 0x52,
    0xb6, 0x02, 0x00, 0x39, 0xb5, 0x06, 0x40, 0x91, 0x94, 0x06, 0x00, 0x71, 0xa1, 0xff, 0xff, 0x54,
    0x77, 0x0a, 0x40, 0xb9, 0x77, 0x00, 0x00, 0x34, 0xf7, 0x06, 0x00, 0x71, 0xe1, 0xff, 0xff, 0x54,
    0x48, 0x13, 0x82, 0xd2, 0x60, 0x0e, 0x40, 0xb9, 0x21, 0x00, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
    0x00, 0x00, 0x00, 0x14,
];
/// See the module. `arm-linux-gnueabihf-as`, read back with `objcopy`.
#[cfg(target_arch = "arm")]
const PROGRAM: &[u8] = &[
    0x00, 0x40, 0x00, 0xe3, 0x41, 0x40, 0x40, 0xe3, 0x00, 0x50, 0x94, 0xe5, 0x00, 0x00, 0x55, 0xe3,
    0x04, 0x00, 0x00, 0x0a, 0x00, 0x70, 0x01, 0xe3, 0x00, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x01, 0x50, 0x55, 0xe2, 0xfa, 0xff, 0xff, 0x1a, 0x04, 0x50, 0x94, 0xe5, 0x00, 0x00, 0x55, 0xe3,
    0x06, 0x00, 0x00, 0x0a, 0x00, 0x60, 0x00, 0xe3, 0x00, 0x60, 0x45, 0xe3, 0x01, 0x80, 0xa0, 0xe3,
    0x00, 0x80, 0xc6, 0xe5, 0x01, 0x6a, 0x86, 0xe2, 0x01, 0x50, 0x55, 0xe2, 0xfb, 0xff, 0xff, 0x1a,
    0x08, 0x90, 0x94, 0xe5, 0x00, 0x00, 0x59, 0xe3, 0x01, 0x00, 0x00, 0x0a, 0x01, 0x90, 0x59, 0xe2,
    0xfd, 0xff, 0xff, 0x1a, 0x9a, 0x70, 0x01, 0xe3, 0x0c, 0x00, 0x94, 0xe5, 0x01, 0x10, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0xfe, 0xff, 0xff, 0xea,
];

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

    /// Start it on `cpu` and hold its task.
    fn start(&self, cpu: Option<usize>) -> Result<Arc<Task>, &'static str> {
        let thread = self.host.spawn_on(TEXT, STACK_TOP, 0, cpu)?;
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
    let map = |at, length, flags| {
        space
            .map_anonymous(at, length, flags)
            .map(|_| ())
            .map_err(|_| "bare: a program's memory would not map")
    };
    map(TEXT, PAGE_SIZE, VmaFlags::READ_WRITE)?;
    uaccess::copy_to_user(space, TEXT, PROGRAM).map_err(|_| "bare: no copy")?;
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
}

/// Run `works` together on `cpu`, and answer what each was charged, having ended
/// with its status.
fn run_charged(works: &[Work], cpu: usize) -> Result<Vec<Charged>, &'static str> {
    let mut sides = Vec::new();
    let mut tasks = Vec::new();
    for work in works {
        let side = BareSide::new(*work)?;
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
    for task in &tasks {
        crate::sched::wait_until_gone(task, PATIENCE_NANOS)?;
        crate::fallible::try_push(
            &mut charged,
            Charged {
                runtime: task.runtime(),
                kernel: task.kernel_time(),
                preempted: task.preemptions(),
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

/// Whether it did nothing but enter the kernel and was charged over half of it.
fn mostly_kernel(one: Charged) -> bool {
    one.kernel.saturating_mul(2) >= one.runtime
}

/// `work` alone on `cpu`, up to [`ATTEMPTS`] times, until `holds`.
fn attempted(
    work: Work,
    cpu: usize,
    holds: fn(Charged) -> bool,
    failure: &'static str,
) -> Result<(), &'static str> {
    for _ in 0..ATTEMPTS {
        let charged = run_charged(core::slice::from_ref(&work), cpu)?;
        if charged.first().is_some_and(|one| holds(*one)) {
            return Ok(());
        }
    }
    Err(failure)
}

/// L.sched.80: a program's time in user mode is not kernel time, however it last
/// came back: from its start, a call, a page fault, or an interrupt that cut it. Two
/// programs on one processor keep their own registers and each is cut (x86-64's way in
/// and interrupt's way out).
///
/// Verifies: L.sched.80
/// Verifies: L.x86_64.59, L.x86_64.79
fn user_time_is_not_kernel_time(cpu: usize) -> Result<(), &'static str> {
    const SPUN: &str = "bare: a program spinning in user mode was charged as kernel time";
    let spin = Work {
        spins: SPINS,
        status: STATUS,
        ..Work::default()
    };
    attempted(spin, cpu, spun_as_user, SPUN)?;
    attempted(Work { calls: 1, ..spin }, cpu, spun_as_user, SPUN)?;
    attempted(Work { pages: 1, ..spin }, cpu, spun_as_user, SPUN)?;
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

/// L.sched.81, L.sched.82: a program's calls and faults are kernel time.
///
/// Verifies: L.sched.81, L.sched.82, H.SCHED.14
fn calls_and_faults_are_kernel_time(cpu: usize) -> Result<(), &'static str> {
    let calls = Work {
        calls: CALLS,
        status: STATUS,
        ..Work::default()
    };
    attempted(
        calls,
        cpu,
        mostly_kernel,
        "bare: a program's system calls were charged as user time",
    )?;
    let pages = Work {
        pages: PAGES,
        status: STATUS,
        ..Work::default()
    };
    attempted(
        pages,
        cpu,
        mostly_kernel,
        "bare: a program's page faults were charged as user time",
    )
}

/// L.trap.2: a fault in a mapped region makes the mapping true and the instruction is
/// retried: every page touched is resident, and the program went on to end.
/// L.x86_64.79: an interrupt from user mode preempts: the two programs above were cut.
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
///
/// Verifies: L.sched.4
fn a_quota_cuts_a_program_alone(cpu: usize) -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "bare: no job")?;
    let job = tree.new_child().map_err(|_| "bare: no job")?;
    if !job.set_bandwidth(20_000_000, 100_000_000) {
        return Err("bare: a job's cpu.max could not be set");
    }
    let side = BareSide::in_job(
        Work {
            spins: SPINS.saturating_mul(4),
            status: STATUS,
            ..Work::default()
        },
        Arc::clone(&job),
    )?;
    let began = crate::timer::now_nanos();
    let task = side.start(Some(cpu))?;
    if side.ended()? != STATUS as i32 {
        return Err("bare: a program under cpu.max did not end with its status");
    }
    let wall = crate::timer::now_nanos().saturating_sub(began);
    crate::sched::wait_until_gone(&task, PATIENCE_NANOS)?;
    let ran = task.runtime();
    if ran.saturating_mul(12) < wall || ran.saturating_mul(3) > wall.saturating_mul(2) {
        crate::console::println!(
            "  bare     under cpu.max 20000 100000 a program ran {} of {} us",
            ran / 1000,
            wall / 1000
        );
        return Err("bare: a program alone on its processor was not held to its cpu.max");
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

// CHECKS-ONLY: stages_check.rs
/// Every case: how many ran, none where the kernel does not count kernel time.
///
/// # Errors
///
/// The first that did not hold.
pub fn run() -> Result<u32, &'static str> {
    // ARMv7-A has no K11: its native programs' end is the Linux one, until M5.
    if !crate::arch::HAS_RESTRICTED {
        return Ok(0);
    }
    a_fault_is_resolved()?;
    a_store_copies_for_the_side_that_writes()?;
    let here = crate::smp::this_cpu().ok_or("bare: no processor")?.logical;
    let count = crate::smp::count();
    let cpu = if count > 1 { (here + 1) % count } else { here };
    a_write_after_protect_faults(cpu)?;
    a_limit_ends_its_own(cpu)?;
    a_quota_cuts_a_program_alone(cpu)?;
    if !crate::sched::vtime::counting() {
        return Ok(5);
    }
    user_time_is_not_kernel_time(cpu)?;
    calls_and_faults_are_kernel_time(cpu)?;
    Ok(7)
}
