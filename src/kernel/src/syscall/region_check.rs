//! Stage 9's check of regions and restricted mode (`docs/microkernel/K1.md`
//! §10, `docs/microkernel/K11.md` §8): the kernel's half of `restricted-check`.
//!
//! The check makes a region as the partition manager will: a job, a region, a
//! process of it; maps the program at the region's window by handle; starts
//! it; and gives it the region, its job and its restricted space over a
//! channel. The program drives its cases and exits with 0 or the failing case;
//! the check answers what only the kernel can see: a sweep of the restricted
//! root, and the barrier count.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Elf;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{MAP_EXECUTE, MAP_READ, MAP_WRITE};

use crate::discovery;
use crate::object::Object;
use crate::object::job::Job;
use crate::object::quota::Resource;
use crate::syscall::native_side_check::NativeSide as Side;
use crate::user::space::AddressSpace;

/// The program.
const PROGRAM: &[u8] = b"/sbin/restricted-check";
/// Where the check's own buffers are.
const BUFFER_AT: u64 = 0x6000_0000;
/// How long the buffers are.
const BUFFER_BYTES: u64 = 0x2_0000;
/// A message's bytes.
const MESSAGE_AT: u64 = BUFFER_AT;
/// A message's handles.
const HANDLES_AT: u64 = BUFFER_AT + 0x100;
/// A read's record.
const ACTUAL_AT: u64 = BUFFER_AT + 0x200;
/// A deadline.
const DEADLINE_AT: u64 = BUFFER_AT + 0x300;
/// What a wait saw.
const OBSERVED_AT: u64 = BUFFER_AT + 0x308;
/// A call's out word.
const OUT_AT: u64 = BUFFER_AT + 0x310;
/// A process's status.
const STATUS_AT: u64 = BUFFER_AT + 0x320;
/// A name.
const NAME_AT: u64 = BUFFER_AT + 0x330;
/// A segment's bytes on their way into the region.
const CHUNK_AT: u64 = BUFFER_AT + 0x1000;
/// One `space_write`.
const CHUNK_BYTES: usize = 0x1_0000;
/// The program's stack in the region: under its image.
const STACK_TOP: u64 = ferrix_native_abi::restricted::SHARED_BASE + 0x40_0000;
/// How long the stack is.
const STACK_BYTES: u64 = 0x10_0000;
/// How long the check waits for the program: long, since a control's case
/// waits out a deadline of its own first.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// What the program asks. As `cases.rs` in `restricted-check`.
const ASK_SWEEP: u8 = 1;
/// Read the barrier count.
const ASK_COUNT_START: u8 = 2;
/// Compare the barrier count.
const ASK_COUNT_END: u8 = 3;
/// Pin the thread whose handle came with this.
const ASK_PIN: u8 = 4;
/// The barrier count must have moved.
const ASK_COUNT_MOVED: u8 = 5;
/// Nothing: a blocking round trip.
const ASK_NOTHING: u8 = 6;
/// `ASK_PIN`'s word for no pin.
const ANY_PROCESSOR: u64 = u64::MAX;
/// Whether the thread whose handle comes with it runs on processor `word` now.
const ASK_ON_PROCESSOR: u8 = 12;
/// Whether there are two processors to read a shootdown across.
const ASK_TWO_PROCESSORS: u8 = 13;
/// The counter's reading now, for the check to scale the row by.
const ASK_ROW_START: u8 = 8;
/// The counter's ticks since [`ASK_ROW_START`].
const ASK_ROW_TICKS: u8 = 9;
/// The row's median of p50s, in ticks.
const ASK_ROW_P50: u8 = 10;
/// In the first message: spin until killed.
const ASK_WAIT_TO_BE_KILLED: u8 = 7;
/// Whether this machine answers armed calls by a stub.
const ASK_WINDOW_ON: u8 = 14;
/// Read the trip and decline counts.
const ASK_WINDOW_START: u8 = 15;
/// The trips since then must be the word.
const ASK_WINDOW_TRIPS: u8 = 16;
/// Trips and declines since then must both be above zero (ledger 6998, X9).
const ASK_WINDOW_BOTH: u8 = 17;
/// Read how many windows there are.
const ASK_WINDOW_LIVE: u8 = 18;
/// There must be as many as then, within the check's patience.
const ASK_WINDOW_WENT: u8 = 19;
/// Read the window's pages by software, from here, many times.
const ASK_WINDOW_READERS: u8 = 20;
/// Whether this build has K1's fast path: `restricted-check`'s cases of it (ledger 7216).
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
const ASK_FAST_BUILT: u8 = 40;
/// Read the asking thread's counts of fast trips.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
const ASK_FAST_START: u8 = 41;
/// Whether count `word >> 56` of the asking thread moved by the rest of the word.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
const ASK_FAST_MOVED: u8 = 42;
/// Set the asking thread's normal mode's `FS` base in its record: the word.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
const ASK_NORMAL_FS: u8 = 43;
/// Set its `GS` base.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
const ASK_NORMAL_GS: u8 = 44;

/// What the check measured, for the boot log.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Sweeps of a restricted root that found nothing of the region.
    pub(crate) sweeps: u32,
    /// The leaves the last one walked.
    pub(crate) leaves: u64,
    /// Barriers counted at exits, against what the program's trips asked.
    pub(crate) barriers: u64,
    /// The restricted row: a Linux number's round trip through restricted mode,
    /// the median of five p50s, in nanoseconds (`MICROKERNEL.md` §7).
    pub(crate) row_ns: u64,
    /// The exits the program counted across those barriers, whatever they decided.
    pub(crate) exits: u64,
    /// Whether those were issued, not only decided: a model without `IBPB`
    /// decides and issues none.
    pub(crate) issued: bool,
}

/// Stage 9's check of regions and restricted mode, said on its line. A kernel of any
/// shape runs it: under `personality=user` from the first processes' start.
///
/// Halts rather than returning, as every other stage's check does.
pub fn check() {
    if !crate::checks::run() {
        return;
    }
    #[cfg(ferrix_personality = "user")]
    match the_old_calls_are_gone() {
        Ok(()) => crate::println!(
            "  regions  process_create and process_start answer ENOSYS: no image is parsed in ring 0"
        ),
        Err(problem) => crate::panic::fatal!(
            crate::panic::catalog::STAGE9_REGIONS,
            "stage 9 region and restricted-mode check failed: {problem}"
        ),
    }
    if !crate::arch::HAS_RESTRICTED {
        crate::println!("  regions  none on this architecture: K1's and K11's calls answer ENOSYS");
        return;
    }
    let free = crate::mm::free_frames();
    let ran = run();
    if cfg!(ferrix_personality = "user") {
        // What it leaves in a kernel about to seal: said in frames (ledger 7216 H.4).
        let _ = crate::sched::wait_until_reaper_quiet(1_000_000_000);
        crate::println!(
            "  regions  the check left {} frames of {free} behind",
            free.saturating_sub(crate::mm::free_frames())
        );
    }
    match ran {
        Ok(report) => crate::println!(
            "  regions  restricted-check held every case: {} sweep of a restricted root over \
             {} leaves found nothing of the region; {} barriers on {} exits under `{}`{}; \
             restricted-getppid {} ns",
            report.sweeps,
            report.leaves,
            report.barriers,
            report.exits,
            if cfg!(ferrix_partition_speculation = "partition") {
                "partition"
            } else {
                "exit"
            },
            if cfg!(ferrix_partition_speculation = "partition") {
                " (none at an exit, by `partition`)"
            } else if report.issued {
                ""
            } else {
                " (decided: this processor model has no barrier to issue)"
            },
            report.row_ns,
        ),
        Err(problem) => crate::panic::fatal!(
            crate::panic::catalog::STAGE9_REGIONS,
            "stage 9 region and restricted-mode check failed: {problem}"
        ),
    }
}

/// L.object.295: in a kernel with no load, the two calls that parsed an image
/// are gone. On a process of its own, which makes nothing: both are refused
/// before any argument is read, and the process ends before `check` goes on.
///
/// Verifies: L.object.295
#[cfg(ferrix_personality = "user")]
fn the_old_calls_are_gone() -> Result<(), &'static str> {
    let side = Side::new()?;
    let answers = [nr::PROCESS_CREATE, nr::PROCESS_START].map(|number| side.call(number, &[0; 4]));
    side.end(crate::object::job::KILLED_STATUS);
    if answers
        .iter()
        .any(|answer| *answer != Err(ferrix_linux_abi::errno::Errno::ENOSYS))
    {
        return Err("build self-check failed: a call that parsed an image still answers");
    }
    Ok(())
}

/// The window's slot's first address, as the case aims at it (`mm::WINDOW_ROOT_SLOT`).
#[cfg(ferrix_restricted_window = "on")]
const WINDOW_AT: u64 = 255 << 39;
/// Reads of it a round, each short of a stub's run, so that some fall inside one.
#[cfg(ferrix_restricted_window = "on")]
const READS_A_ROUND: u32 = 2_000;

/// A handle as a register.
const fn reg(handle: Handle) -> u64 {
    handle.0 as u64
}

/// Run the check.
///
/// Verifies: H.TRAP.21, H.TRAP.22, H.TRAP.23, H.TRAP.24, H.OBJ.23, H.OBJ.24
/// Verifies: L.aarch64.80
/// Verifies: H.OBJ.25, H.OBJ.26, L.trap.14, L.trap.15, L.trap.16, L.trap.17
/// Verifies: L.trap.18, L.trap.19, L.trap.20, L.trap.21, L.trap.22, L.trap.23
/// Verifies: L.trap.24, L.trap.25, L.trap.26, L.trap.27, L.trap.28, `L.x86_64.260`
/// Verifies: `L.x86_64.261`, `L.x86_64.262`, `L.x86_64.263`, `L.x86_64.264`, `L.x86_64.265`, `L.x86_64.266`
/// Verifies: `L.x86_64.267`, `L.x86_64.269`, `L.x86_64.270`, `L.x86_64.271`, `L.x86_64.276`, L.object.270
/// Verifies: L.object.271, L.object.272, L.object.273, L.object.274, L.object.275, L.object.276
/// Verifies: L.object.277, L.object.278, L.object.279, L.object.280, L.object.281, L.object.282
/// Verifies: L.user.193
/// Verifies: L.trap.30, L.user.190, L.user.191, L.user.192
/// Verifies: `L.x86_64.272`, `L.x86_64.273`, `L.x86_64.274`, `L.x86_64.275`, `L.x86_64.282`
/// Verifies: L.user.180, L.user.181, L.user.182, L.user.183, L.user.184
/// Verifies: L.aarch64.70, L.aarch64.71, L.aarch64.72, L.aarch64.73, L.aarch64.74
///
/// # Errors
///
/// The first thing that did not hold.
fn run() -> Result<Report, &'static str> {
    let side = Side::new()?;
    let _ = side
        .space()
        .map_anonymous(BUFFER_AT, BUFFER_BYTES, ferrix_vma::VmaFlags::READ_WRITE)
        .map_err(|_| "no room for the check's buffers")?;

    // As the partition manager will: a job, a region, a process of it.
    let root = Job::new_root().map_err(|_| "no memory for the check's job")?;
    let root_handle = side
        .with_handles(|table| table.insert(Object::Job(Arc::clone(&root)), Rights::JOB))
        .map_err(|_| "no room for the check's job")?;
    let job = side.handle(nr::JOB_CREATE, &[reg(root_handle), 0], "job_create failed")?;
    refusals(&side, job)?;
    let region = side.handle(
        nr::REGION_CREATE,
        &[reg(job), OUT_AT],
        "region_create failed",
    )?;
    let region_space = Handle(side.get_u32(OUT_AT)?);
    side.put(NAME_AT, b"restricted-check")?;
    let process = side.handle(
        nr::PROCESS_CREATE_SHARED,
        &[reg(region), reg(job), 0, NAME_AT, 16, OUT_AT],
        "process_create_shared failed",
    )?;
    let space = Handle(side.get_u32(OUT_AT)?);
    let restricted = space_of(&side, space)?;
    let shared = space_of(&side, region_space)?;
    // A region's space is forked by nothing (`L.user.184`).
    if side.call(nr::SPACE_CLONE, &[reg(region_space)]).is_ok() {
        return Err("space_clone of a region's space was not refused");
    }
    if restricted.ceiling() != ferrix_native_abi::restricted::SHARED_BASE {
        return Err("a restricted space's ceiling is not the region's floor");
    }

    let entry = load_program(&side, region_space)?;

    // What the program is given: the region, its job, its restricted space.
    let _ = side
        .call(nr::CHANNEL_CREATE, &[OUT_AT])
        .map_err(|_| "channel_create failed")?;
    let ours = Handle(side.get_u32(OUT_AT)?);
    let theirs = Handle(side.get_u32(OUT_AT + 4)?);
    let job_given = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(job), u64::from(SAME_RIGHTS)],
        "could not duplicate the job",
    )?;
    let space_given = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(region_space), u64::from(SAME_RIGHTS)],
        "could not duplicate the region's space",
    )?;
    let mut given = [0_u8; 16];
    given[..4].copy_from_slice(&region.0.to_ne_bytes());
    given[4..8].copy_from_slice(&job_given.0.to_ne_bytes());
    given[8..12].copy_from_slice(&space.0.to_ne_bytes());
    given[12..].copy_from_slice(&space_given.0.to_ne_bytes());
    side.put(HANDLES_AT, &given)?;
    // Its one byte: whether this kernel compiled `PROCESS_START` out (K15 §2.2).
    side.put(
        MESSAGE_AT,
        &[if cfg!(ferrix_personality = "user") {
            0x52
        } else {
            0
        }],
    )?;
    let _ = side
        .call(
            nr::CHANNEL_WRITE,
            &[reg(ours), MESSAGE_AT, 1, HANDLES_AT, 4],
        )
        .map_err(|_| "could not give the program its handles")?;
    let _ = side
        .call(
            nr::PROCESS_START_SHARED,
            &[reg(process), entry, STACK_TOP, reg(theirs)],
        )
        .map_err(|_| "process_start_shared failed")?;

    let mut report = Report::default();
    serve(&side, ours, &restricted, &shared, &mut report)?;

    // How it ended is the diagnosis.
    wait(&side, process, Signals::TERMINATED)
        .map_err(|()| "the program did not end within the check's patience")?;
    let _ = side
        .call(nr::PROCESS_STATUS, &[reg(process), STATUS_AT])
        .map_err(|_| "process_status failed")?;
    let state = side.get_u32(STATUS_AT)?;
    let status = side.get_u32(STATUS_AT + 4)?;
    if state != ferrix_native_abi::types::PROCESS_EXITED || status != 0 {
        crate::console::println!(
            "  regions  restricted-check ended in state {state}: case {status} did not hold"
        );
        return Err("a case of restricted-check did not hold: its number is on the line above");
    }
    if report.sweeps == 0 {
        return Err("the program never asked for its sweep");
    }
    fault_ends_the_region(&side, root_handle)?;
    Ok(report)
}

/// A thread's window went with it, and its frames and charge with that (ledger 6787 W11): back
/// to the count before it was made, once its handle is gone.
fn window_went(before: u64) -> bool {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while crate::arch::windows_live() != before {
        if crate::timer::now_nanos() > deadline {
            crate::console::println!(
                "  rwindow  {} windows where {before} were before the thread",
                crate::arch::windows_live()
            );
            return false;
        }
        let _ = crate::sched::reap();
        crate::sched::yield_now();
    }
    true
}

/// A fault of a process of a region in normal mode ends the region's job, not
/// the one process (K11 §5, the consultant's N3): two processes of a fresh
/// region, one waiting and one that loads from a hole; and what they held goes
/// back (K11 §8).
///
/// Verifies: L.trap.24, L.object.282
fn fault_ends_the_region(side: &Side, root: Handle) -> Result<(), &'static str> {
    let job = side.handle(nr::JOB_CREATE, &[reg(root), 0], "job_create failed")?;
    let region = side.handle(
        nr::REGION_CREATE,
        &[reg(job), OUT_AT],
        "region_create failed",
    )?;
    let space = Handle(side.get_u32(OUT_AT)?);
    let entry = load_program(side, space)?;
    let made = |name: &[u8]| -> Result<(Handle, Handle), &'static str> {
        side.put(NAME_AT, name)?;
        let process = side.handle(
            nr::PROCESS_CREATE_SHARED,
            &[reg(region), reg(job), 0, NAME_AT, name.len() as u64, OUT_AT],
            "process_create_shared failed",
        )?;
        Ok((process, Handle(side.get_u32(OUT_AT)?)))
    };
    let (waiting, waiting_space) = made(b"waits")?;
    let (faulting, faulting_space) = made(b"faults")?;
    let _ = side
        .call(nr::CHANNEL_CREATE, &[OUT_AT])
        .map_err(|_| "channel_create failed")?;
    let ours = Handle(side.get_u32(OUT_AT)?);
    let theirs = Handle(side.get_u32(OUT_AT + 4)?);
    side.put(MESSAGE_AT, &[ASK_WAIT_TO_BE_KILLED])?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(ours), MESSAGE_AT, 1, 0, 0])
        .map_err(|_| "could not tell a process to wait")?;
    let _ = side
        .call(
            nr::PROCESS_START_SHARED,
            &[reg(waiting), entry, STACK_TOP, reg(theirs)],
        )
        .map_err(|_| "could not start the waiting process")?;
    // A stack of its own, under the first's.
    let _ = side
        .call(
            nr::SPACE_ALLOCATE,
            &[
                reg(space),
                STACK_TOP - 2 * STACK_BYTES,
                STACK_BYTES,
                u64::from(MAP_READ | MAP_WRITE),
            ],
        )
        .map_err(|_| "no second stack")?;
    let _ = side
        .call(
            nr::PROCESS_START_SHARED,
            &[reg(faulting), entry, STACK_TOP - STACK_BYTES, 0],
        )
        .map_err(|_| "could not start the faulting process")?;
    wait(side, faulting, Signals::TERMINATED)
        .map_err(|()| "a process that faulted in normal mode did not end")?;
    wait(side, waiting, Signals::TERMINATED)
        .map_err(|()| "a fault in normal mode left its region's other process running")?;
    // What they held goes back, once the last handles do.
    let job_object = side.with_handles(|table| match table.get(job) {
        Ok((Object::Job(job), _)) => Some(Arc::clone(job)),
        _ => None,
    });
    let job_object = job_object.ok_or("the job's handle names no job")?;
    for handle in [
        region,
        space,
        waiting_space,
        faulting_space,
        waiting,
        faulting,
        ours,
    ] {
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
    }
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let held = job_object
            .usage(Resource::Memory)
            .map_or(0, |usage| usage.used);
        let objects = job_object
            .usage(Resource::Objects)
            .map_or(0, |usage| usage.used);
        if held == 0 && objects == 0 {
            return Ok(());
        }
        if crate::timer::now_nanos() > deadline {
            crate::console::println!(
                "  regions  a region's job still holds {held} bytes and {objects} objects"
            );
            return Err("a region and its processes did not give back what they held");
        }
        let _ = crate::sched::reap();
        crate::sched::yield_now();
    }
}

/// The refusals the kernel can see from outside a region (`L.object.270`,
/// `L.object.271`): a job without `MANAGE`.
fn refusals(side: &Side, job: Handle) -> Result<(), &'static str> {
    let weak = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(job), u64::from(Rights::WAIT.0)],
        "could not duplicate the job",
    )?;
    if side.call(nr::REGION_CREATE, &[reg(weak), OUT_AT]).is_ok() {
        return Err("region_create without MANAGE on the job was not refused");
    }
    Ok(())
}

/// The space a handle names.
fn space_of(side: &Side, handle: Handle) -> Result<Arc<AddressSpace>, &'static str> {
    side.with_handles(|table| {
        let Ok((Object::Space(space), _)) = table.get(handle) else {
            return Err("a space's handle does not name a space");
        };
        Ok(Arc::clone(space.space()))
    })
}

/// Map the program's segments and a stack at the region's window, by handle,
/// as K15's builder will: its entry.
fn load_program(side: &Side, space: Handle) -> Result<u64, &'static str> {
    let file = discovery::devmgr::read_file(PROGRAM).map_err(|_| "no /sbin/restricted-check")?;
    let bytes: &[u8] = &file;
    let elf = Elf::parse(bytes).map_err(|_| "restricted-check is no ELF")?;
    let page = |address: u64| address & !(PAGE_SIZE - 1);
    let mut segments: Vec<(u64, u64, bool, bool)> = Vec::new();
    for segment in elf.segments() {
        if !segment.is_load() {
            continue;
        }
        let start = page(segment.vaddr);
        let end = (segment.vaddr + segment.memsz).next_multiple_of(PAGE_SIZE);
        let _ = side
            .call(
                nr::SPACE_ALLOCATE,
                &[
                    reg(space),
                    start,
                    end - start,
                    u64::from(MAP_READ | MAP_WRITE),
                ],
            )
            .map_err(|_| "space_allocate of a segment failed")?;
        let data = bytes
            .get(segment.offset as usize..(segment.offset + segment.filesz) as usize)
            .ok_or("a segment past the file's end")?;
        let mut done = 0;
        while done < data.len() {
            let chunk = data
                .get(done..data.len().min(done + CHUNK_BYTES))
                .unwrap_or(&[]);
            side.put(CHUNK_AT, chunk)?;
            let _ = side
                .call(
                    nr::SPACE_WRITE,
                    &[
                        reg(space),
                        segment.vaddr + done as u64,
                        CHUNK_AT,
                        chunk.len() as u64,
                    ],
                )
                .map_err(|_| "space_write of a segment failed")?;
            done += chunk.len();
        }
        segments
            .try_reserve(1)
            .map_err(|_| "no memory for the segments")?;
        segments.push((
            start,
            end - start,
            segment.is_writable(),
            segment.is_executable(),
        ));
    }
    for (start, length, writable, executable) in segments {
        let protection = MAP_READ
            | if writable { MAP_WRITE } else { 0 }
            | if executable { MAP_EXECUTE } else { 0 };
        let _ = side
            .call(
                nr::SPACE_PROTECT,
                &[reg(space), start, length, u64::from(protection)],
            )
            .map_err(|_| "space_protect of a segment failed")?;
    }
    let _ = side
        .call(
            nr::SPACE_ALLOCATE,
            &[
                reg(space),
                STACK_TOP - STACK_BYTES,
                STACK_BYTES,
                u64::from(MAP_READ | MAP_WRITE),
            ],
        )
        .map_err(|_| "space_allocate of the stack failed")?;
    Ok(elf.entry())
}

/// Wait for a signal on `handle`, within the check's patience.
fn wait(side: &Side, handle: Handle, signals: Signals) -> Result<(), ()> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    side.put(DEADLINE_AT, &deadline.to_ne_bytes())
        .map_err(|_| ())?;
    side.call(
        nr::OBJECT_WAIT_ONE,
        &[reg(handle), u64::from(signals.0), DEADLINE_AT, OBSERVED_AT],
    )
    .map(|_| ())
    .map_err(|_| ())
}

/// Pin the thread whose handle came with the last message to processor
/// `word`, modulo how many there are: its way out moves it there.
fn pin(side: &Side, word: u64) -> Result<bool, &'static str> {
    let handle = Handle(side.get_u32(HANDLES_AT)?);
    let task = side.with_handles(|table| match table.get(handle) {
        Ok((Object::Thread(thread), _)) => thread.task(),
        _ => None,
    });
    let Some(task) = task else {
        return Ok(false);
    };
    // Every processor again for `ANY_PROCESSOR`, which a thread pinned for a case asks.
    let mask = if word == ANY_PROCESSOR {
        let mut all = ferrix_sched_class::CpuSet::of(0);
        for cpu in 1..crate::smp::count() {
            let _ = all.insert(cpu);
        }
        all
    } else {
        ferrix_sched_class::CpuSet::of(usize::try_from(word).unwrap_or(0) % crate::smp::count())
    };
    let _ = crate::sched::set_affinity(&task, &mask);
    if let Ok((gone, _)) = side.with_handles(|table| table.remove(handle)) {
        crate::object::dispose([gone]);
    }
    Ok(true)
}

/// Whether the thread named by the message's handle is on processor `word`.
fn on_processor(side: &Side, word: u64) -> Result<bool, &'static str> {
    let handle = Handle(side.get_u32(HANDLES_AT)?);
    let task = side.with_handles(|table| match table.get(handle) {
        Ok((Object::Thread(thread), _)) => thread.task(),
        _ => None,
    });
    if let Ok((gone, _)) = side.with_handles(|table| table.remove(handle)) {
        crate::object::dispose([gone]);
    }
    Ok(task.is_some_and(|task| task.cpu() as u64 == word))
}

/// What the window's asks remember between them.
#[derive(Default)]
struct WindowAsks {
    /// Trips and declines when last asked to read them.
    counts: (u64, u64),
    /// Windows when last asked to read them.
    live: u64,
}

/// The program's asks about the window (`restricted_window`, ledger 6787, 6998).
fn window_asked(
    what: u8,
    word: u64,
    restricted: &Arc<AddressSpace>,
    window: &mut WindowAsks,
) -> bool {
    match what {
        ASK_WINDOW_ON => crate::arch::window_on(),
        ASK_WINDOW_LIVE => {
            window.live = crate::arch::windows_live();
            true
        }
        ASK_WINDOW_WENT => window_went(window.live),
        ASK_WINDOW_READERS => window_readers_refused(restricted),
        ASK_WINDOW_START => {
            window.counts = crate::arch::window_counts();
            true
        }
        ASK_WINDOW_TRIPS => {
            let (trips, declines) = crate::arch::window_counts();
            let (trips, declines) = (trips - window.counts.0, declines - window.counts.1);
            let held = if word == u64::MAX {
                trips > 0
            } else {
                trips == word
            };
            if !held {
                crate::console::println!(
                    "  rwindow  {trips} trips, {declines} declines, where the case wanted {word}"
                );
            }
            held
        }
        ASK_WINDOW_BOTH => {
            let (trips, declines) = crate::arch::window_counts();
            let (trips, declines) = (trips - window.counts.0, declines - window.counts.1);
            crate::console::println!(
                "  rwindow  overlapping threads: {trips} trips by stub, {declines} declined to \
                 two roots"
            );
            trips > 0 && declines > 0
        }
        _ => false,
    }
}

/// A reader of the slot with no bit up, many times: refused each time.
#[cfg(ferrix_restricted_window = "on")]
#[allow(
    clippy::disallowed_methods,
    reason = "this check is what the method is for"
)]
fn window_readers_refused(restricted: &Arc<AddressSpace>) -> bool {
    (0..READS_A_ROUND).all(|_| restricted.window_readers_refuse(WINDOW_AT, WINDOW_AT + PAGE_SIZE))
}

/// No window, nothing to read.
#[cfg(not(ferrix_restricted_window = "on"))]
const fn window_readers_refused(_restricted: &Arc<AddressSpace>) -> bool {
    true
}

/// Whether the image's rule is `partition`: the case of a switch between speculation
/// domains (ledger 7216 T.1).
#[cfg(ferrix_partition_speculation = "partition")]
const ASK_DOMAIN_BUILT: u8 = 47;
/// Keep this thread off processor 0, and read processor 0's counts.
#[cfg(ferrix_partition_speculation = "partition")]
const ASK_DOMAIN_START: u8 = 48;
/// Whether processor 0's counts say the word's crossings between domains happened, each
/// with its barrier. The word's top bit: inside one domain, none, and the refill.
#[cfg(ferrix_partition_speculation = "partition")]
const ASK_DOMAIN_END: u8 = 49;
/// The top bit of [`ASK_DOMAIN_END`]'s word.
#[cfg(ferrix_partition_speculation = "partition")]
const DOMAIN_WITHIN: u64 = 1 << 63;
/// Its next bit: the trips were to go direct.
#[cfg(ferrix_partition_speculation = "partition")]
const DOMAIN_DIRECT: u64 = 1 << 62;
/// Whether the image has direct switches, for the same two cases.
#[cfg(ferrix_partition_speculation = "partition")]
const ASK_DOMAIN_DIRECT: u8 = 50;

/// Processor 0's decisions, invalidations and refills, and the direct switches.
#[cfg(ferrix_partition_speculation = "partition")]
fn domain_counts() -> [u64; 4] {
    #[cfg(ferrix_fast_ipc)]
    let direct = crate::sched::direct::counts()[0];
    #[cfg(not(ferrix_fast_ipc))]
    let direct = 0;
    [
        crate::arch::barrier_decisions_on(0),
        crate::arch::switch_barriers_on(0),
        crate::arch::refills_in_domain_on(0),
        direct,
    ]
}

/// What [`ASK_DOMAIN_END`] asks, on the line whichever way it goes.
#[cfg(ferrix_partition_speculation = "partition")]
fn domain_end(started: [u64; 4], word: u64) -> bool {
    let now = domain_counts();
    let [decided, issued, refilled, direct] = {
        let mut moved = now;
        for (count, then) in moved.iter_mut().zip(started) {
            *count -= then;
        }
        moved
    };
    let within = word & DOMAIN_WITHIN != 0;
    let wanted = word & !(DOMAIN_WITHIN | DOMAIN_DIRECT);
    let direct_held = word & DOMAIN_DIRECT == 0 || direct >= wanted / 4;
    let held = if within {
        decided == 0
            && issued == 0
            && (refilled >= wanted || !crate::arch::refill_wanted_in_domain())
    } else {
        // Each with its invalidation where this processor offers one, read from the same
        // word the switch reads: none and "decided only" only where it does not.
        decided >= wanted
            && if crate::arch::speculation_check::switch_barrier_offered() {
                issued == decided
            } else {
                issued == 0
            }
    };
    crate::console::println!(
        "  regions  {} {wanted} switches on a processor: {decided} barriers decided, {issued} issued{}, \
         {refilled} in-domain refills, {direct} of the trips direct",
        if within {
            "within one domain,"
        } else {
            "across two domains,"
        },
        if !crate::arch::speculation_check::switch_barrier_offered() {
            " (this processor offers none: decided only)"
        } else {
            ""
        },
    );
    held && direct_held
}

/// Whether the image has reservations and direct switches: case 197 (ledger 7088, 7134 (d)).
#[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
const ASK_EDF_BUILT: u8 = 51;
/// Read the count of direct switches, and each way it declined at T12.
#[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
const ASK_EDF_START: u8 = 52;
/// Whether it moved by the word.
#[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
const ASK_EDF_MOVED: u8 = 53;
/// Whether it did not move, and T12 did by the word.
#[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
const ASK_EDF_DECLINED: u8 = 54;

/// Read the count of direct switches, where the image has them.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
const ASK_DIRECT_START: u8 = 45;
/// Whether it moved by the word; yes in an image without them, where the case's call
/// goes the general way.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
const ASK_DIRECT_MOVED: u8 = 46;

/// `sched::direct`'s trips, as far as this image has them.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
fn direct_trips() -> Option<u64> {
    #[cfg(ferrix_fast_ipc)]
    {
        Some(crate::sched::direct::counts()[0])
    }
    #[cfg(not(ferrix_fast_ipc))]
    {
        None
    }
}

/// The task of the thread whose handle came with the ask, the handle gone.
#[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
fn asking_task(side: &Side) -> Result<Option<Arc<crate::sched::Task>>, &'static str> {
    let handle = Handle(side.get_u32(HANDLES_AT)?);
    let task = side.with_handles(|table| match table.get(handle) {
        Ok((Object::Thread(thread), _)) => thread.task(),
        _ => None,
    });
    if let Ok((gone, _)) = side.with_handles(|table| table.remove(handle)) {
        crate::object::dispose([gone]);
    }
    Ok(task)
}

/// [`ASK_COUNT_END`]: the barriers since [`ASK_COUNT_START`] against the word's exits.
#[cfg_attr(
    not(all(ferrix_fast_restricted, ferrix_checks = "on")),
    allow(unused_variables, reason = "its use is under the same cfg")
)]
fn count_end(started: (u64, u64), refills_started: u64, word: u64, report: &mut Report) -> bool {
    let (decided, issued) = crate::trap::restricted_barriers();
    report.barriers = decided - started.0;
    report.exits = word;
    report.issued = issued - started.1 == word;
    // What was issued against what the processor offers, not against the
    // exit's own answer: a machine with no IBPB issues none.
    let decided_here = if crate::trap::exit_barrier_built() {
        word
    } else {
        0
    };
    let held =
        report.barriers == decided_here && report.issued == crate::trap::exit_barrier_offered();
    // Where the root write empties nothing, each of them refilled (6747 (1d)).
    #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
    let held = held && {
        let (refills, emptied) = crate::arch::fast_check::exit_refills();
        let refills = refills - refills_started;
        if decided_here > 0 {
            crate::console::println!(
                "  regions  {refills} refills at {decided_here} exits, the root \
                 write {} the return predictor",
                if emptied { "emptying" } else { "not emptying" }
            );
        }
        emptied || refills == decided_here
    };
    if !held {
        // A shortfall is a missed barrier; over is read as a preempted trip
        // (`docs/backlog/flakes.md`, fl-093). Which it was must be on the line.
        crate::console::println!(
            "  restricted barrier count: {} decided, {} wanted, issued {}, offered {}",
            report.barriers,
            decided_here,
            report.issued,
            crate::trap::exit_barrier_offered()
        );
    }
    held
}

/// What the asks of a build's own features read at their start.
#[derive(Default)]
struct FeatureCounts {
    /// [`ASK_EDF_START`]'s.
    #[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
    edf_started: [u64; 2],
    /// [`ASK_DOMAIN_START`]'s.
    #[cfg(ferrix_partition_speculation = "partition")]
    domain_started: [u64; 4],
    /// [`ASK_FAST_START`]'s.
    #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
    fast_started: [u64; 3],
    /// [`ASK_DIRECT_START`]'s.
    #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
    direct_started: u64,
}

/// The asks only some builds answer: no on any other, as an unknown ask (ledger 7216, 7249).
#[cfg_attr(
    not(any(
        ferrix_partition_speculation = "partition",
        all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc)
    )),
    allow(
        unused_variables,
        clippy::needless_pass_by_ref_mut,
        clippy::unnecessary_wraps,
        reason = "the arms that use them are this build's"
    )
)]
fn feature_ask(
    side: &Side,
    what: u8,
    word: u64,
    counts_from: &mut FeatureCounts,
) -> Result<bool, &'static str> {
    Ok(match what {
            #[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
            ASK_EDF_BUILT => true,
            #[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
            ASK_EDF_START => {
                let counts = crate::sched::direct::counts();
                counts_from.edf_started = [counts[0], counts[13]];
                true
            }
            #[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
            ASK_EDF_MOVED => crate::sched::direct::counts()[0] - counts_from.edf_started[0] >= word,
            #[cfg(all(ferrix_sched_deadline = "edf_cbs", ferrix_fast_ipc))]
            ASK_EDF_DECLINED => {
                let counts = crate::sched::direct::counts();
                let (trips, declined) = (counts[0] - counts_from.edf_started[0], counts[13] - counts_from.edf_started[1]);
                crate::console::println!(
                    "  regions  a reserved thread's channel calls: {trips} of them direct, {declined} declined at T12"
                );
                trips == 0 && declined >= word
            }
            #[cfg(ferrix_partition_speculation = "partition")]
            ASK_DOMAIN_BUILT => true,
            #[cfg(ferrix_partition_speculation = "partition")]
            ASK_DOMAIN_START => {
                if let Some(me) = crate::sched::current() {
                    let mut away = ferrix_sched_class::CpuSet::of(1 % crate::smp::count());
                    let _ = away.insert(crate::smp::count() - 1);
                    let _ = crate::sched::set_affinity(&me, &away);
                    crate::sched::yield_now();
                }
                counts_from.domain_started = domain_counts();
                true
            }
            #[cfg(ferrix_partition_speculation = "partition")]
            ASK_DOMAIN_END => {
                if let Some(me) = crate::sched::current() {
                    let mut all = ferrix_sched_class::CpuSet::of(0);
                    for cpu in 1..crate::smp::count() {
                        let _ = all.insert(cpu);
                    }
                    let _ = crate::sched::set_affinity(&me, &all);
                }
                domain_end(counts_from.domain_started, word)
            }
            #[cfg(ferrix_partition_speculation = "partition")]
            ASK_DOMAIN_DIRECT => cfg!(ferrix_fast_ipc),
            #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
            ASK_FAST_BUILT => true,
            #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
            ASK_FAST_START => match asking_task(side)? {
                Some(task) => {
                    counts_from.fast_started = crate::arch::fast_check::restricted_fast_counts(&task);
                    // The fast path reads per-processor tables of this size by a logical
                    // number with no bound check (ledger 6747 (3c)).
                    crate::smp::count() <= ferrix_sched_class::MAX_CPUS
                }
                None => false,
            },
            #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
            ASK_FAST_MOVED => asking_task(side)?.is_some_and(|task| {
                let counts = crate::arch::fast_check::restricted_fast_counts(&task);
                let which = usize::from((word >> 56) as u8);
                let want = word & 0x00FF_FFFF_FFFF_FFFF;
                let held = counts
                    .get(which)
                    .zip(counts_from.fast_started.get(which))
                    .is_some_and(|(now, then)| now - then >= want);
                if !held {
                    crate::console::println!(
                        "  restricted fast trips: count {which} wanted to move by {want}: {counts:?} from {:?}",
                        counts_from.fast_started
                    );
                }
                held
            }),
            #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
            ASK_DIRECT_START => {
                counts_from.direct_started = direct_trips().unwrap_or(0);
                true
            }
            #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
            ASK_DIRECT_MOVED => direct_trips().is_none_or(|trips| trips - counts_from.direct_started >= word),
            #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
            ASK_NORMAL_FS => asking_task(side)?.is_some_and(|task| {
                crate::arch::fast_check::restricted_set_normal_bases_for_check(&task, Some(word), None);
                true
            }),
            #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
            ASK_NORMAL_GS => asking_task(side)?.is_some_and(|task| {
                crate::arch::fast_check::restricted_set_normal_bases_for_check(&task, None, Some(word));
                true
            }),
        _ => false,
    })
}

/// Answer what the program asks until it closes its end.
fn serve(
    side: &Side,
    channel: Handle,
    restricted: &Arc<AddressSpace>,
    shared: &Arc<AddressSpace>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let mut started = (0, 0);
    let mut row_started = 0;
    let mut row_scale = (0, 1);
    let mut window = WindowAsks::default();
    let mut features = FeatureCounts::default();
    #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
    let mut refills_started = 0_u64;
    loop {
        let readable = Signals(Signals::READABLE.0 | Signals::PEER_CLOSED.0);
        wait(side, channel, readable)
            .map_err(|()| "the program went silent past the check's patience")?;
        let read = side.call(
            nr::CHANNEL_READ,
            &[reg(channel), MESSAGE_AT, 9, HANDLES_AT, 1, ACTUAL_AT],
        );
        if read.is_err() {
            return Ok(());
        }
        let message = side.get(MESSAGE_AT, 9)?;
        let word = message
            .get(1..9)
            .and_then(|word| <[u8; 8]>::try_from(word).ok())
            .map_or(0, u64::from_ne_bytes);
        let what = message.first().copied().unwrap_or(0);
        let yes = match what {
            ASK_SWEEP => {
                let leaves = crate::mm::check::sweep_restricted(restricted, shared)?;
                #[allow(
                    clippy::disallowed_methods,
                    reason = "this check is what the method is for"
                )]
                restricted.check_normal_slots()?;
                report.sweeps += 1;
                report.leaves = leaves;
                true
            }
            ASK_COUNT_START => {
                started = crate::trap::restricted_barriers();
                #[cfg(all(ferrix_fast_restricted, ferrix_checks = "on"))]
                {
                    refills_started = crate::arch::fast_check::exit_refills().0;
                }
                true
            }
            ASK_COUNT_END => {
                #[cfg(not(all(ferrix_fast_restricted, ferrix_checks = "on")))]
                let refills_started = 0;
                count_end(started, refills_started, word, report)
            }
            ASK_PIN => pin(side, word)?,
            ASK_ON_PROCESSOR => on_processor(side, word)?,
            ASK_TWO_PROCESSORS => crate::smp::count() >= 2,
            ASK_COUNT_MOVED => {
                !crate::trap::exit_barrier_built()
                    || crate::trap::restricted_barriers().0 > started.0
            }
            ASK_NOTHING => true,
            ASK_WINDOW_ON..=ASK_WINDOW_READERS => window_asked(what, word, restricted, &mut window),
            ASK_ROW_START => {
                row_started = crate::timer::now_nanos();
                true
            }
            ASK_ROW_TICKS => {
                row_scale = (crate::timer::now_nanos() - row_started, word);
                true
            }
            ASK_ROW_P50 => {
                report.row_ns = u128::from(word)
                    .saturating_mul(u128::from(row_scale.0))
                    .checked_div(u128::from(row_scale.1))
                    .map_or(0, |ns| u64::try_from(ns).unwrap_or(u64::MAX));
                true
            }
            what => feature_ask(side, what, word, &mut features)?,
        };
        side.put(MESSAGE_AT, &[u8::from(yes)])?;
        let _ = side
            .call(nr::CHANNEL_WRITE, &[reg(channel), MESSAGE_AT, 1, 0, 0])
            .map_err(|_| "could not answer the program")?;
    }
}
