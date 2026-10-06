//! Stage 9's self-checks: two processes, a channel between them, and the
//! rules a capability system lives by.
//!
//! The host tests in `src/lib/kernel/objects` prove the handle table and the queue in
//! isolation. What they cannot prove is the join: that a handle written by one
//! process really leaves its table and arrives in another's naming the *same*
//! object, that a system call refused half-way leaves both tables as they
//! were, and that the objects a closed channel was still holding give their
//! memory back. Those are properties of the kernel's handlers over real
//! address spaces, and nothing short of running them can show them.
//!
//! Every call goes through [`native::dispatch`] with raw registers, so the
//! number decoding and the argument order are exercised as a program will
//! exercise them — not the handler functions called by name.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::sync::SpinLock;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    CHANNEL_MAX_BYTES, MAP_READ, MAP_WRITE, PACKET_INTERRUPT, PACKET_SIGNAL, PACKET_USER,
};
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::device::{self, DeviceNode};
use crate::mm;
use crate::object::channel::{Endpoint, ReadError};
use crate::object::interrupt;
use crate::object::job::{self, Job, KILLED_STATUS};
use crate::object::process::ProcessRef;
use crate::object::{self, Object};
use crate::sched::Task;
use crate::syscall::check::spinner;
use crate::syscall::image;
use crate::syscall::process::{self, Process};
use crate::syscall::{self as linux, Outcome, SyscallArgs, native, uaccess};
use crate::user::memory_type;
use crate::user::space::{Access, Destination, FileMapping, FilePlace, SpaceError, WindowPages};
use crate::user::vmo::Vmo;
use ferrix_elf::Class;

/// Where each check process keeps its buffers. Stage 10's ring check borrows
/// the second page of it.
pub(crate) const SCRATCH: u64 = 0x4000_0000;
/// Two pages of it.
const SCRATCH_LEN: u64 = 2 * PAGE_SIZE;
/// Bytes being sent, and bytes being written into a VMO.
const PAYLOAD: u64 = SCRATCH;
/// Bytes received.
const INBOX: u64 = SCRATCH + 0x100;
/// Handle values, in either direction.
const HANDLES: u64 = SCRATCH + 0x200;
/// `channel_create`'s two handles.
const PAIR: u64 = SCRATCH + 0x300;
/// A `ReadActual`.
const ACTUAL: u64 = SCRATCH + 0x308;
/// A VMO offset.
const OFFSET: u64 = SCRATCH + 0x310;
/// A VMO size.
const SIZE: u64 = SCRATCH + 0x318;
/// A wait's deadline.
const DEADLINE: u64 = SCRATCH + 0x320;
/// The signals a wait observed.
const OBSERVED: u64 = SCRATCH + 0x328;
/// An `IoMappingSpec`.
const SPEC: u64 = SCRATCH + 0x330;
/// A `PortPacket`.
const PACKET_AT: u64 = SCRATCH + 0x340;
/// A registration's key.
const KEY: u64 = SCRATCH + 0x360;
/// A pin's device addresses.
const PINNED_AT: u64 = SCRATCH + 0x370;
/// Where the mapping check maps its VMO: clear of the scratch region.
const MAPPED: u64 = 0x5000_0000;

/// How long the waker sleeps before it writes.
const WAKE_AFTER_NANOS: u64 = 20_000_000;
/// How long a check waits for anything before calling it lost.
const PATIENCE_NANOS: u64 = 120_000_000_000;
/// How long the spinning programs run before a job is killed under them.
const KILL_AFTER_NANOS: u64 = 20_000_000;
/// How many deliveries the wake check makes through each kind of wait.
const WAKE_ROUNDS: u32 = 8;
/// How much later each round of the wake check delivers than the one before:
/// the wait's five-millisecond recheck period, divided by [`WAKE_ROUNDS`].
const WAKE_STAGGER_NANOS: u64 = 625_000;
/// Rounds of one kind the wake check needs ended by the delivery's wake: a
/// majority, because a wait whose task was slow to block can find its
/// interrupt already pending and never sleep.
const WOKEN_ROUNDS_REQUIRED: u32 = WAKE_ROUNDS / 2 + 1;

/// What travels in the VMO, to show the handle that arrives names it.
const SECRET: &[u8] = b"carried by a handle";
/// What travels in the message.
const PING: &[u8] = b"ping";

/// What the checks measured, for the boot log.
#[derive(Debug)]
pub(crate) struct Report {
    /// Messages carried from one process to the other and read back intact.
    pub(crate) messages: u32,
    /// Handles that left one process's table and arrived in the other's.
    pub(crate) moved: u32,
    /// Calls refused with exactly the status they had to be refused with.
    pub(crate) refusals: u32,
    /// Frames the second run did not give back. Zero, or something leaks.
    pub(crate) leaked: i64,
    /// Waits woken by the thing they waited for, rather than their deadline.
    pub(crate) woken: u32,
    /// Packets taken from a port, user and signal alike.
    pub(crate) packets: u32,
    /// Processes ended by killing a job they were in.
    pub(crate) killed: u32,
    /// Messages two programs in user mode exchanged with each other.
    pub(crate) exchanged: u32,
    /// Programs a process made from a VMO and started through a handle, whose
    /// ends it heard through that handle.
    pub(crate) spawned: u32,
    /// Processes nobody started, ended when their last handle went.
    pub(crate) abandoned: u32,
}

/// Counts what happened, so the report is a measurement and not a claim.
#[derive(Debug, Default)]
struct Counter {
    /// See [`Report::messages`].
    messages: u32,
    /// See [`Report::moved`].
    moved: u32,
    /// See [`Report::refusals`].
    refusals: u32,
    /// See [`Report::woken`].
    woken: u32,
    /// See [`Report::packets`].
    packets: u32,
    /// See [`Report::killed`].
    killed: u32,
    /// See [`Report::exchanged`].
    exchanged: u32,
    /// See [`Report::spawned`].
    spawned: u32,
    /// See [`Report::abandoned`].
    abandoned: u32,
    /// See [`Report::mapped`].
    mapped: u32,
    /// See [`DeviceReport::combined`].
    combined: u32,
    /// See [`Report::interrupts`].
    interrupts: u32,
    /// See [`DeviceReport::pinned`].
    pinned: u32,
    /// See [`DeviceReport::delivered`].
    delivered: u32,
    /// See [`DeviceReport::wakes`].
    wakes: u32,
    /// See [`DeviceReport::slowest_wake`].
    slowest_wake: u64,
    /// See [`DeviceReport::coalesced`].
    coalesced: u32,
    /// See [`DeviceReport::storm_nanos`].
    storm_nanos: u64,
}

/// Run them. `Err` names the first thing that was not true.
pub(crate) fn run() -> Result<Report, &'static str> {
    check_the_native_range_is_not_a_linux_one()?;
    // First of the channel checks, so a read that stops answering from its
    // queue fails here, on this rule, and not in a check built on it.
    check_messages_before_the_close_are_read_first()?;
    crate::console::println!(
        "  lastmsg  3 messages a peer wrote before closing read in order, then its close"
    );

    // Twice, measured on the second, for the reason `syscall::check::run`
    // gives: the heap keeps a page of each size class the first run touched.
    let _warm = check_two_processes()?;
    check_a_vmo_maps_as_shared_memory(&mut Counter::default())?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let mut counter = check_two_processes()?;
    check_a_vmo_maps_as_shared_memory(&mut counter)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    // Checked, not only printed. The cycle check's own premise is that this
    // count is what fails if a refusal stops happening, and a count nothing
    // tested would boot green through exactly that.
    if leaked != 0 {
        mm::print_frame_delta("native", leaked);
        window.report("native");
        return Err("the native object checks did not give every frame back");
    }

    // Outside the measured window, both: a woken waker and a killed program
    // leave kernel stacks for the scheduler to reap later, and the frame count
    // would read a stack not yet reaped as a leak.
    let mut after = Counter::default();
    check_a_wait_is_woken_by_what_it_waits_for(&mut after)?;
    let write_read = object::write_read_check::run()?;
    crate::console::println!(
        "  wrread   channel_write_read through the entry: {} answers zero past their message, {} \
         refusals left registers 2 to 4 as sent, {} messages read in order through the slot, {} \
         waits ended by a message, the peer's close and a kill, each within {} s",
        write_read.zeroed,
        write_read.kept,
        write_read.ordered,
        write_read.woken,
        write_read.within_seconds
    );
    if write_read.sync_checked {
        crate::console::println!(
            "  sync     {} of {} Sync writes from another processor moved the reader onto the \
             writer's processor; a reader pinned to its own was woken there",
            write_read.moved,
            object::write_read_check::SYNC_ROUNDS
        );
    } else {
        crate::console::println!(
            "  sync     not checked: one processor, nowhere to move a reader to"
        );
    }
    let fast = object::fast_path_check::run()?;
    if cfg!(target_arch = "x86_64") {
        crate::console::println!(
            "  fastcase {} cases of the fast path's tests answered as the general path answers \
             them{}, every waiter within {} s; the fast path {}: {} trips taken",
            fast.cases,
            if fast.two_processors { ", one across two processors" } else { "" },
            10,
            if fast.on { "on" } else { "off" },
            fast.trips
        );
    } else {
        crate::console::println!(
            "  fastcase not checked: the fast path is x86-64's; no fast path counter moved"
        );
    }
    check_a_job_kill_takes_down_a_process_tree(&mut after)?;
    check_a_long_chain_of_jobs_is_freed_without_recursion()?;
    check_a_job_counts_its_members()?;
    check_the_two_kills()?;
    check_a_port_wait_is_woken_by_a_message(&mut after)?;
    check_two_programs_talk_over_a_channel(&mut after)?;
    check_a_program_ended_by_its_fault_is_heard_and_freed(&mut after)?;
    check_a_process_starts_another(&mut after)?;
    let edges = object::edge_check::run()?;
    crate::console::println!(
        "  edges    {edges} object paths no program takes: subtree control, a node's owner, a \
         packet put back, a registration passed over, a cycle walk meeting one end twice, a \
         delivery to a line nobody holds"
    );
    let domains = object::domain_check::run()?;
    if domains.hardened {
        crate::console::println!(
            "  domain   {} switches inside one speculation domain skipped the barrier, {} out of \
             one kept it: none across domains, none by a mover, a leaver or a sharer; a leaver \
             barriered at once here{}{}",
            domains.skipped,
            domains.kept,
            if domains.elsewhere {
                " and where its thread ran"
            } else {
                ", on a machine with one processor"
            },
            if domains.late {
                ", and where its domain was recorded after the leave's scan"
            } else {
                ""
            }
        );
    } else {
        crate::console::println!(
            "  domain   not checked: built --mitigations off, no switch decides a barrier"
        );
    }
    check_the_wake_row()?;
    let formatted = object::format_check::run()?;
    crate::console::println!(
        "  format   {formatted} errors and objects formatted for a diagnostic, each under its \
         own name, through every lock it holds without waiting for one"
    );

    Ok(Report {
        messages: counter.messages,
        moved: counter.moved,
        refusals: counter.refusals + after.refusals,
        leaked,
        woken: after.woken,
        packets: counter.packets + after.packets,
        killed: after.killed,
        exchanged: after.exchanged,
        spawned: after.spawned,
        abandoned: after.abandoned,
    })
}

/// The wake row (`sched::work::check`), and its line.
fn check_the_wake_row() -> Result<(), &'static str> {
    // And a channel end's state word, which 2e's wait reads in place of the
    // inbox: beside the wake row, the other half of that wait's argument.
    let words = super::channel::check::run()?;
    crate::console::println!(
        "  chword   {words} channel operations in turn on one pair, each end's state word its \
         inbox's after every one, and a close marked in the survivor's"
    );
    match crate::sched::work::check::run()? {
        Some(report) => crate::console::println!(
            "  wakerow  a wake made while its target held its own run-queue lock waited for the \
             lock and woke it (try {}): no wake reads a task's state without its home's lock",
            report.tries
        ),
        None => crate::console::println!(
            "  wakerow  not checked: one processor, nowhere for the poster to run"
        ),
    }
    Ok(())
}

/// Where the process-creation check stages a child's image: apart from the
/// scratch region, whose first page holds staged values an image would
/// overwrite.
const IMAGE_AT: u64 = 0x5000_0000;

/// The name every child of the process-creation check is made with.
const SPAWN_NAME: &[u8] = b"spawned";

/// A process makes another from a VMO, starts it through its handle with a
/// bootstrap handle, and hears it end through a port. The ways that must not
/// work do not.
///
/// The starter is a check process whose calls go through the real native
/// dispatch, which is what `devmgr` will make them through. The child is a
/// real program in user mode on every architecture that exits with its first
/// argument register. So its status is the value its bootstrap handle has in
/// its own table, and a child started with no bootstrap exits 0.
///
/// What must not happen:
/// - a start without `MANAGE`, or a second start;
/// - a start of a child whose job was killed first, which must also leave the
///   bootstrap with the starter under the same value;
/// - an unstarted child outliving its only handle: closed, it ends the child,
///   whose watch fires, and once the reaper is quiet nothing holds it;
/// - an unstarted child outliving a channel message carrying its handle: the
///   channel closed with the message unread ends it too.
fn check_a_process_starts_another(counter: &mut Counter) -> Result<(), &'static str> {
    if arch::USER_ARGUMENT_PROGRAM.is_empty() {
        return Ok(());
    }
    let side = Side::new()?;
    let spawner = Spawner::new(&side)?;
    check_what_process_create_refuses(&spawner, counter)?;
    check_process_create_survives_each_failure(&spawner)?;
    check_a_child_finds_its_bootstrap(&spawner, counter)?;
    check_a_killed_child_is_not_started(&spawner, counter)?;
    check_an_unstarted_child_ends_with_its_handles(&spawner, counter)?;
    side.close_everything();
    Ok(())
}

/// A check process set up to make children: a job, a VMO holding the child's
/// image, its name staged, and a port to hear them end on.
struct Spawner<'a> {
    /// The process making the calls.
    side: &'a Side,
    /// A root job, with every right a job carries.
    root: Handle,
    /// The VMO holding a child's ELF image.
    image: Handle,
    /// Where watches on children fire.
    port: Handle,
}

impl<'a> Spawner<'a> {
    /// Build the image of `arch::USER_ARGUMENT_PROGRAM`, write it into a VMO
    /// through `vmo_write`, and give `side` a job and a port.
    fn new(side: &'a Side) -> Result<Spawner<'a>, &'static str> {
        let job = Job::new_root().map_err(|_| "no memory for the starter's job")?;
        let root = side
            .process
            .with_handles(|table| table.insert(Object::Job(job), Rights::JOB))
            .map_err(|_| "no room for the starter's job")?;
        let class = if size_of::<usize>() == 8 {
            Class::Elf64
        } else {
            Class::Elf32
        };
        let file = image::build_with(
            class,
            arch::ARCH.elf_machine(),
            image::Shape::Good,
            arch::USER_ARGUMENT_PROGRAM,
        );
        let _ = side
            .process
            .space()
            .map_anonymous(
                IMAGE_AT,
                len(&file).div_ceil(PAGE_SIZE) * PAGE_SIZE,
                VmaFlags::READ_WRITE,
            )
            .map_err(|_| "could not map room for a child's image")?;
        side.put(IMAGE_AT, &file)?;
        let image = side.handle(
            nr::VMO_CREATE,
            &[len(&file)],
            "vmo_create for a child's image failed",
        )?;
        side.put_offset(0)?;
        let _ = side
            .call(nr::VMO_WRITE, &[reg(image), IMAGE_AT, len(&file), OFFSET])
            .map_err(|_| "writing a child's image into a VMO failed")?;
        side.put(INBOX, SPAWN_NAME)?;
        let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
        Ok(Spawner {
            side,
            root,
            image,
            port,
        })
    }

    /// `process_create` in `job` from `vmo`, with the staged name cut to
    /// `name_len`.
    fn create(&self, job: Handle, vmo: Handle, name_len: u64) -> Result<usize, Errno> {
        self.side
            .call(nr::PROCESS_CREATE, &[reg(job), reg(vmo), INBOX, name_len])
    }

    /// A child made in `job` from the real image.
    fn made(&self, job: Handle, what: &'static str) -> Result<Handle, &'static str> {
        let value = self
            .create(job, self.image, len(SPAWN_NAME))
            .map_err(|_| what)?;
        u32::try_from(value).map(Handle).map_err(|_| what)
    }

    /// Watch `target` for `TERMINATED` on the port, with `key`.
    fn watch(&self, target: Handle, key: u64, what: &'static str) -> Result<(), &'static str> {
        self.side.put(KEY, &key.to_ne_bytes())?;
        self.side
            .call(
                nr::OBJECT_WAIT_ASYNC,
                &[
                    reg(target),
                    reg(self.port),
                    u64::from(Signals::TERMINATED.0),
                    KEY,
                ],
            )
            .map(|_| ())
            .map_err(|_| what)
    }

    /// Wait for the watch with `key` to fire, and count its packet.
    fn heard(
        &self,
        key: u64,
        what: &'static str,
        counter: &mut Counter,
    ) -> Result<(), &'static str> {
        stage_deadline(self.side, PATIENCE_NANOS)?;
        let _ = self
            .side
            .call(nr::PORT_WAIT, &[reg(self.port), DEADLINE, PACKET_AT])
            .map_err(|_| what)?;
        let (fired, kind, signals, _, _) = read_packet(self.side)?;
        if fired != key
            || kind != PACKET_SIGNAL
            || !Signals(signals).intersects(Signals::TERMINATED)
        {
            return Err(what);
        }
        counter.packets += 1;
        Ok(())
    }

    /// The exit status of the process `handle` names, once it has one.
    fn status_of(&self, handle: Handle) -> Option<i32> {
        self.side
            .process
            .with_handles(|table| match table.get(handle) {
                Ok((Object::Process(child), _)) => child.exit_status(),
                _ => None,
            })
    }

    /// A second handle to `handle` carrying only `rights`.
    fn narrowed(
        &self,
        handle: Handle,
        rights: Rights,
        what: &'static str,
    ) -> Result<Handle, &'static str> {
        self.side.handle(
            nr::HANDLE_DUPLICATE,
            &[reg(handle), u64::from(rights.0)],
            what,
        )
    }
}

/// `process_create` needs `MANAGE` on the job and `READ` on the image, a job
/// where the job goes, a name within the limit, and an ELF image in the VMO.
fn check_what_process_create_refuses(
    spawner: &Spawner<'_>,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let Spawner {
        side, root, image, ..
    } = *spawner;
    let name_len = len(SPAWN_NAME);
    let blind_job = spawner.narrowed(
        root,
        Rights::WAIT,
        "duplicating a job without MANAGE failed",
    )?;
    refused(
        spawner.create(blind_job, image, name_len),
        status::ACCESS_DENIED,
        "process_create without MANAGE on the job was not refused",
        counter,
    )?;
    let unreadable = spawner.narrowed(
        image,
        Rights::WRITE,
        "duplicating a VMO without READ failed",
    )?;
    refused(
        spawner.create(root, unreadable, name_len),
        status::ACCESS_DENIED,
        "process_create from a VMO it may not read was not refused",
        counter,
    )?;
    refused(
        spawner.create(image, image, name_len),
        status::WRONG_TYPE,
        "process_create in a VMO instead of a job was not refused",
        counter,
    )?;
    refused(
        spawner.create(
            root,
            image,
            u64::try_from(nr::PROCESS_NAME_MAX + 1).unwrap_or(u64::MAX),
        ),
        status::INVALID_ARGS,
        "a process name over the limit was taken",
        counter,
    )?;
    let blank = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    refused(
        spawner.create(root, blank, name_len),
        status::INVALID_ARGS,
        "a VMO holding no ELF image was loaded as one",
        counter,
    )
}

/// `process_create` run once per allocation it makes, the `n`th failing on
/// the `n`th run alone (`user::alloc_check`): each run makes the child or
/// answers `NO_MEMORY`, and a child made is closed unstarted, which ends it.
fn check_process_create_survives_each_failure(spawner: &Spawner<'_>) -> Result<(), &'static str> {
    let task =
        crate::sched::current_id().ok_or("the process-creation check runs outside a task")?;
    for nth in 1..=2000 {
        // The heap's allocations alone: the load code `process_create` runs
        // still allocates infallibly in places (MEMORY-AND-TIMING.md §1.3 --
        // the signal tables no longer), and a frame refused under one of
        // those stops the kernel.
        crate::fallible::inject_once(task, nth, false);
        let made = spawner.create(spawner.root, spawner.image, len(SPAWN_NAME));
        let failed = crate::fallible::stop_injecting() > 0;
        match made {
            Ok(value) => {
                let handle = u64::try_from(value).unwrap_or(u64::MAX);
                let _ = spawner
                    .side
                    .call(nr::HANDLE_CLOSE, &[handle])
                    .map_err(|_| "closing a child made with an allocation failing failed")?;
            }
            Err(status::NO_MEMORY) if failed => {}
            Err(other) => {
                crate::console::println!("  spawn    run {nth}: status {}", other.0);
                return Err(
                    "process_create failed otherwise than for memory, one allocation failing",
                );
            }
        }
        if !failed {
            return Ok(());
        }
    }
    Err("process_create made more allocations than its check allows")
}

/// A child started with a channel end finds the end's value in its first
/// argument register, the end leaves the starter and closes with the child,
/// and a child started with no bootstrap finds zero. A start without
/// `MANAGE`, and a second start, are refused.
fn check_a_child_finds_its_bootstrap(
    spawner: &Spawner<'_>,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let side = spawner.side;
    let child = spawner.made(spawner.root, "process_create of a real image failed")?;
    spawner.watch(child, 71, "watching a made process failed")?;
    let (near, far) = side.channel()?;
    let manage_less = spawner.narrowed(
        child,
        Rights::WAIT,
        "duplicating a process handle without MANAGE failed",
    )?;
    refused(
        side.call(nr::PROCESS_START, &[reg(manage_less), reg(far)]),
        status::ACCESS_DENIED,
        "process_start without MANAGE was not refused",
        counter,
    )?;
    // A bootstrap the move refuses -- here one already closed -- is refused
    // after the child's task is prepared, so the prepared start is dropped
    // on the way out: the start given back, which the start below then takes.
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(manage_less)])
        .map_err(|_| "closing a narrowed process handle failed")?;
    refused(
        side.call(nr::PROCESS_START, &[reg(child), reg(manage_less)]),
        status::BAD_HANDLE,
        "process_start with a closed bootstrap handle was not refused",
        counter,
    )?;
    let _ = side
        .call(nr::PROCESS_START, &[reg(child), reg(far)])
        .map_err(|_| "process_start after a refused bootstrap move failed")?;
    refused(
        side.call(nr::HANDLE_CLOSE, &[reg(far)]),
        status::BAD_HANDLE,
        "a started process's bootstrap stayed in its starter's table",
        counter,
    )?;
    refused(
        side.call(nr::PROCESS_START, &[reg(child), 0]),
        status::BAD_STATE,
        "a process was started twice",
        counter,
    )?;
    spawner.heard(
        71,
        "a started process's end did not fire the watch on its handle",
        counter,
    )?;
    if spawner.status_of(child) != i32::try_from(BOOTSTRAP.0).ok() {
        crate::console::println!(
            "  spawn    a started process ended with {:?}, not its bootstrap's value {}",
            spawner.status_of(child),
            BOOTSTRAP.0
        );
        return Err("a started process did not find its bootstrap handle's value on entry");
    }
    // The end that was moved went with the child's table, not the starter's.
    stage_deadline(side, PATIENCE_NANOS)?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[
                reg(near),
                u64::from(Signals::PEER_CLOSED.0),
                DEADLINE,
                OBSERVED,
            ],
        )
        .map_err(|_| "the bootstrap end did not close with the process it was moved into")?;
    counter.spawned += 1;

    let bare = spawner.made(spawner.root, "process_create of a second child failed")?;
    spawner.watch(bare, 72, "watching a second made process failed")?;
    let _ = side
        .call(nr::PROCESS_START, &[reg(bare), 0])
        .map_err(|_| "process_start with no bootstrap failed")?;
    spawner.heard(
        72,
        "a process started with no bootstrap was not heard to end",
        counter,
    )?;
    if spawner.status_of(bare) != Some(0) {
        return Err("a process started with no bootstrap did not find zero on entry");
    }
    counter.spawned += 1;
    Ok(())
}

/// A child whose job is killed before its start is refused the start, and the
/// bootstrap stays with the starter under the value it had; the killed job
/// takes no new process.
fn check_a_killed_child_is_not_started(
    spawner: &Spawner<'_>,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let side = spawner.side;
    let doomed_job = side.handle(nr::JOB_CREATE, &[reg(spawner.root)], "job_create failed")?;
    let doomed = spawner.made(doomed_job, "process_create in a live job failed")?;
    let (_kept_near, kept_far) = side.channel()?;
    let _ = side
        .call(nr::JOB_KILL, &[reg(doomed_job)])
        .map_err(|_| "killing a job with an unstarted process in it failed")?;
    refused(
        side.call(nr::PROCESS_START, &[reg(doomed), reg(kept_far)]),
        status::BAD_STATE,
        "a process killed before its start was started",
        counter,
    )?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(kept_far)])
        .map_err(|_| "a refused start took its bootstrap from the starter")?;
    refused(
        spawner.create(doomed_job, spawner.image, len(SPAWN_NAME)),
        status::BAD_STATE,
        "a killed job took a new process",
        counter,
    )
}

/// A child nobody started ends when the last handle to it goes: closed, it is
/// heard and, once the reaper is quiet, freed; carried in a message nobody
/// read, it ends when the channel is closed.
///
/// Verifies: L.object.95
fn check_an_unstarted_child_ends_with_its_handles(
    spawner: &Spawner<'_>,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let side = spawner.side;
    let orphan = spawner.made(spawner.root, "process_create of a child to abandon failed")?;
    let alive = side
        .process
        .with_handles(|table| match table.get(orphan) {
            Ok((Object::Process(child), _)) => child
                .control()
                .and_then(|control| control.process::<Process>())
                .map(|process| Arc::downgrade(&process)),
            _ => None,
        })
        .ok_or("a made process's handle has no way back to it")?;
    formats(side, orphan, "Process(ProcessRef {")?;
    spawner.watch(orphan, 73, "watching an unstarted process failed")?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(orphan)])
        .map_err(|_| "closing an unstarted process's handle failed")?;
    spawner.heard(
        73,
        "closing an unstarted process's only handle did not end it",
        counter,
    )?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while alive.strong_count() > 0 {
        if crate::timer::now_nanos() >= deadline {
            return Err("an unstarted process outlived its only handle");
        }
        crate::sched::sleep_for(1_000_000);
    }
    counter.abandoned += 1;

    let carried = spawner.made(spawner.root, "process_create of a child to send failed")?;
    spawner.watch(carried, 74, "watching an unstarted process to send failed")?;
    let (from, to) = side.channel()?;
    side.put_handles(&[carried])?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(from), PAYLOAD, 0, HANDLES, 1])
        .map_err(|_| "sending an unstarted process's handle failed")?;
    for end in [from, to] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(end)])
            .map_err(|_| "closing a channel carrying a process handle failed")?;
    }
    spawner.heard(
        74,
        "an unstarted process whose handle died unread in a channel did not end",
        counter,
    )?;
    counter.abandoned += 1;
    Ok(())
}

/// A program that dies of its own fault is ended by it, heard by whoever
/// watches it, and freed.
///
/// A fault from user mode arrives in a trap, and the kill it forces ends the
/// process there: its handles and descriptors close, what they held is freed,
/// and its watchers fire, none of which may run with interrupts masked. Nothing
/// else in the boot test ends a program that way, which is how a kill run from
/// the trap with interrupts masked went unseen until a reverse-map check made a
/// child write through a read-only page. The program here writes through a null
/// pointer, which faults alike on every architecture, and has to end with
/// `128 + SIGSEGV`; the watch on its handle has to queue `TERMINATED`; and once
/// the reaper has run, nothing may keep the process.
fn check_a_program_ended_by_its_fault_is_heard_and_freed(
    counter: &mut Counter,
) -> Result<(), &'static str> {
    const SEGV_STATUS: i32 = 128 + ferrix_linux_abi::types::SIGSEGV as i32;
    const WATCH_KEY: u64 = 61;

    if arch::USER_FAULT_PROGRAM.is_empty() {
        return Ok(());
    }
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_FAULT_PROGRAM,
    );
    let faulting = process::load(
        &file,
        &[b"/fault"],
        &[],
        [0x3b; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that faults could not be loaded")?;

    let watcher = Side::new()?;
    let handle = watcher
        .process
        .with_handles(|table| {
            table.insert(Object::Process(ProcessRef::new(&faulting)), Rights::PROCESS)
        })
        .map_err(|_| "no room for a handle to a program that faults")?;
    let port = watcher.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    watcher.put(KEY, &WATCH_KEY.to_ne_bytes())?;
    let _ = watcher
        .call(
            nr::OBJECT_WAIT_ASYNC,
            &[
                reg(handle),
                reg(port),
                u64::from(Signals::TERMINATED.0),
                KEY,
            ],
        )
        .map_err(|_| "watching a program that faults failed")?;

    let task =
        process::start(&faulting).map_err(|_| "a program that faults could not be started")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let status = faulting.wait_for_exit(deadline);
    if status != Some(SEGV_STATUS) {
        crate::console::println!(
            "  fault    a program writing through a null pointer ended with {status:?}"
        );
        return Err("a program writing through a null pointer did not end with SIGSEGV");
    }

    // The watch fires after the process has closed its handles, a little
    // after the status above was readable: waited for, not taken at once.
    watcher.put(DEADLINE, &deadline.to_ne_bytes())?;
    let _ = watcher
        .call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET_AT])
        .map_err(|_| "a program ended by its fault did not fire the watch on it")?;
    let (key, kind, signals, _, _) = read_packet(&watcher)?;
    if key != WATCH_KEY
        || kind != PACKET_SIGNAL
        || !Signals(signals).intersects(Signals::TERMINATED)
    {
        return Err("the packet for a program ended by its fault did not say what fired it");
    }
    counter.packets += 1;

    task_stops(&task, deadline)?;
    let alive = Arc::downgrade(&faulting);
    drop(task);
    drop(faulting);
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    if alive.upgrade().is_some() {
        return Err("a program ended by its fault was never freed");
    }
    watcher.close_everything();
    Ok(())
}

/// A native number reaches the native dispatcher and no Linux handler.
///
/// Asked of this build's own table, because a host test cannot know which one
/// the kernel was compiled against. With no process running, the answer has
/// to be `ESRCH` — the native side asking for a process — and not whatever a
/// Linux call of that number would have said.
fn check_the_native_range_is_not_a_linux_one() -> Result<(), &'static str> {
    if arch::decode_syscall(nr::CHANNEL_CREATE).is_some() {
        return Err("this build's Linux table claims a native number");
    }
    let args = SyscallArgs {
        abi: crate::trap::Abi::Native,
        number: nr::CHANNEL_CREATE,
        args: [0; 6],
        ip: 0,
    };
    if linux::dispatch(&args, None) != Outcome::Return(Errno::ESRCH.as_return_value()) {
        return Err("a native call with no process was not refused with ESRCH");
    }
    Ok(())
}

/// Everything, between two fresh processes, with every handle closed after.
fn check_two_processes() -> Result<Counter, &'static str> {
    let mut counter = Counter::default();
    let sender = Side::new()?;
    let receiver = Side::new()?;

    let (near, far) = connect(&sender, &receiver)?;
    let arrived =
        check_a_message_carries_a_handle_across(&sender, &receiver, near, far, &mut counter)?;
    check_rights_only_shrink(&receiver, arrived, &mut counter)?;
    check_a_refused_send_keeps_its_handles(&sender, near, &mut counter)?;
    check_a_cycle_of_channels_is_refused(&sender, &mut counter)?;
    check_an_endpoint_survives_a_bad_buffer(&sender, &mut counter)?;
    check_a_message_is_read_into_a_private_file_mapping(&sender, &mut counter)?;
    check_an_endpoint_is_read_into_pages_shared_by_fork(&sender, &mut counter)?;
    check_ports(&sender, &mut counter)?;
    check_a_port_hears_a_process_end(&sender, &mut counter)?;
    check_a_full_channel_says_wait(&sender, near, &receiver, far, &mut counter)?;
    check_a_closed_peer_frees_what_was_queued(&sender, near, &receiver, far, &mut counter)?;
    if sender.call(0x1037, &[]) != Err(Errno::ENOSYS) {
        return Err("a gap in the native range was not ENOSYS");
    }

    sender.close_everything();
    receiver.close_everything();
    Ok(counter)
}

/// A port hears that a process has ended, after the process has closed its
/// handles, and a handle to it keeps nothing of it but that.
///
/// A watch registered while the process runs fires only when it ends, one
/// registered afterwards fires at once, and the handle says `TERMINATED` from
/// the same moment. A watch on the far end of a channel the process held fires
/// first, so the close came before the packet: what lets `devmgr` reset a
/// device only once its driver's pins are gone. And with nothing but the
/// handle left, the process is freed.
///
/// Verifies: L.object.1, L.object.93, L.object.96
fn check_a_port_hears_a_process_end(
    side: &Side,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let watched = Side::new()?;
    let (_inside, outside) = connect(&watched, side)?;
    let handle = side
        .process
        .with_handles(|table| {
            table.insert(
                Object::Process(ProcessRef::new(&watched.process)),
                Rights::PROCESS,
            )
        })
        .map_err(|_| "no room for a process handle")?;
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    let terminated = u64::from(Signals::TERMINATED.0);
    let watch = |target: Handle, signals: u64, key: u64| {
        side.put(KEY, &key.to_ne_bytes())
            .map_err(|_| Errno::EFAULT)?;
        side.call(
            nr::OBJECT_WAIT_ASYNC,
            &[reg(target), reg(port), signals, KEY],
        )
    };

    let _ =
        watch(handle, terminated, 51).map_err(|_| "watching a process through a port failed")?;
    refused(
        take_now(side, port),
        status::TIMED_OUT,
        "a watch on a running process fired",
        counter,
    )?;
    stage_deadline(side, 0)?;
    refused(
        side.call(
            nr::OBJECT_WAIT_ONE,
            &[reg(handle), terminated, DEADLINE, OBSERVED],
        ),
        status::TIMED_OUT,
        "a running process said it had terminated",
        counter,
    )?;
    let blind = side.handle(
        nr::HANDLE_DUPLICATE,
        &[
            reg(handle),
            u64::from((Rights::DUPLICATE | Rights::TRANSFER).0),
        ],
        "duplicating a process handle without WAIT failed",
    )?;
    refused(
        watch(blind, terminated, 59),
        status::ACCESS_DENIED,
        "a process handle without WAIT was watched",
        counter,
    )?;
    let _ = watch(outside, u64::from(Signals::PEER_CLOSED.0), 53)
        .map_err(|_| "watching a channel the process held failed")?;

    let alive = Arc::downgrade(&watched.process);
    process::kill(&watched.process, 3);
    for (key, signal, what) in [
        (
            53,
            Signals::PEER_CLOSED,
            "a watcher heard of a process's end before its handles closed",
        ),
        (
            51,
            Signals::TERMINATED,
            "a process's end did not fire the watch on it",
        ),
    ] {
        let _ = take_now(side, port).map_err(|_| what)?;
        let (fired, kind, signals, _, _) = read_packet(side)?;
        if fired != key || kind != PACKET_SIGNAL || !Signals(signals).intersects(signal) {
            return Err(what);
        }
        counter.packets += 1;
    }
    stage_deadline(side, 0)?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[reg(handle), terminated, DEADLINE, OBSERVED],
        )
        .map_err(|_| "an ended process did not say it had terminated")?;
    let _ = watch(handle, terminated, 52).map_err(|_| "watching an ended process failed")?;
    let _ = take_now(side, port).map_err(|_| "a watch on an ended process did not fire at once")?;
    if read_packet(side)?.0 != 52 {
        return Err("a watch on an ended process fired with the wrong key");
    }
    counter.packets += 1;

    // Freed at once only because a process made for a check has no task. A
    // started program's task holds its process until the idle loop's reaper
    // lets it go, which `kill` does not wait for: pointed at one, this would
    // have to wait for the reaper first.
    drop(watched);
    if alive.upgrade().is_some() {
        return Err("a handle to an ended process kept the process");
    }
    for closing in [handle, blind, port, outside] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(closing)])
            .map_err(|_| "closing a handle the process check made failed")?;
    }
    Ok(())
}

/// A mapped VMO is the VMO.
///
/// Bytes written into the object are there through the mapping, and bytes
/// written through the mapping are there in the object; a forked space sees
/// the same pages rather than copies; and the mapping outlives the handle it
/// was made with. A protection needs the rights behind it, `mprotect` and
/// `mremap` may not change what `vmo_map` made, and a mapping with no
/// protection, a write-only one, one past the object's end, one that is not
/// whole pages, or one over another mapping is refused.
///
/// Verifies: L.user.75
fn check_a_vmo_maps_as_shared_memory(counter: &mut Counter) -> Result<(), &'static str> {
    let side = Side::new()?;
    let vmo = side.handle(nr::VMO_CREATE, &[2 * PAGE_SIZE], "vmo_create failed")?;
    side.put_offset(PAGE_SIZE + 0x10)?;
    side.put(PAYLOAD, SECRET)?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), PAYLOAD, len(SECRET), OFFSET])
        .map_err(|_| "vmo_write failed")?;

    let both = u64::from(MAP_READ | MAP_WRITE);
    side.put_offset(PAGE_SIZE)?;
    let mapped = side
        .call(nr::VMO_MAP, &[reg(vmo), MAPPED, PAGE_SIZE, both, OFFSET])
        .map_err(|_| "mapping a VMO failed")?;
    if u64::try_from(mapped) != Ok(MAPPED) {
        return Err("a VMO was mapped somewhere other than where it was asked to go");
    }
    if side.get(MAPPED + 0x10, SECRET.len())? != SECRET {
        return Err("bytes written into a VMO are not there through its mapping");
    }
    side.put(MAPPED + 0x40, PING)?;
    side.put_offset(PAGE_SIZE + 0x40)?;
    let _ = side
        .call(nr::VMO_READ, &[reg(vmo), INBOX, len(PING), OFFSET])
        .map_err(|_| "vmo_read failed")?;
    if side.get(INBOX, PING.len())? != PING {
        return Err("bytes written through a mapping are not in the VMO");
    }

    // Shared, so a forked space reaches the same pages, not copies of them.
    let child = side
        .process
        .space()
        .fork()
        .map_err(|_| "forking a space with a mapped VMO failed")?;
    uaccess::copy_to_user(&child, MAPPED + 0x80, b"fork")
        .map_err(|_| "a forked space could not write through its mapping")?;
    if side.get(MAPPED + 0x80, 4)? != b"fork" {
        return Err("a write through a forked mapping did not reach the VMO");
    }
    drop(child);

    let [narrow, no_map] = check_vmo_map_needs_its_rights(&side, vmo, counter)?;
    let [end, other_end] = check_what_vmo_map_refuses(&side, vmo, counter)?;

    for handle in [vmo, narrow, no_map, end, other_end] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(handle)])
            .map_err(|_| "closing a handle the mapping check made failed")?;
    }
    if side.get(MAPPED + 0x10, SECRET.len())? != SECRET {
        return Err("a mapping lost its VMO when the handle it was made with closed");
    }
    side.close_everything();
    Ok(())
}

/// A `vmo_map` protection needs the rights behind it, and `mprotect` and
/// `mremap` may not change a region `vmo_map` made. Returns the two narrowed
/// handles, for the caller to close.
///
/// Verifies: L.user.74
fn check_vmo_map_needs_its_rights(
    side: &Side,
    vmo: Handle,
    counter: &mut Counter,
) -> Result<[Handle; 2], &'static str> {
    let both = u64::from(MAP_READ | MAP_WRITE);
    side.put_offset(0)?;
    let read_only = u64::from((Rights::MAP | Rights::READ).0);
    let narrow = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(vmo), read_only],
        "duplicating a VMO without WRITE failed",
    )?;
    let elsewhere = MAPPED + 4 * PAGE_SIZE;
    refused(
        side.call(
            nr::VMO_MAP,
            &[reg(narrow), elsewhere, PAGE_SIZE, both, OFFSET],
        ),
        status::ACCESS_DENIED,
        "a handle without WRITE mapped a VMO writable",
        counter,
    )?;
    let _ = side
        .call(
            nr::VMO_MAP,
            &[
                reg(narrow),
                elsewhere,
                PAGE_SIZE,
                u64::from(MAP_READ),
                OFFSET,
            ],
        )
        .map_err(|_| "a handle without WRITE could not map a VMO read-only")?;

    // The Linux calls may not reshape what vmo_map made: mprotect would hand
    // the mapping a right the handle did not carry, and mremap would grow the
    // VMO behind it.
    let space = side.process.space();
    if !matches!(
        space.protect(elsewhere, PAGE_SIZE, VmaFlags::READ_WRITE),
        Err(SpaceError::Refused(_))
    ) {
        return Err("mprotect made a read-only vmo_map region writable");
    }
    if !matches!(
        space.remap(elsewhere, PAGE_SIZE, 2 * PAGE_SIZE, Destination::Anywhere),
        Err(SpaceError::Refused(_))
    ) {
        return Err("mremap resized a region vmo_map made");
    }
    counter.refusals += 2;
    let no_map = u64::from((Rights::READ | Rights::WRITE).0);
    let no_map = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(vmo), no_map],
        "duplicating a VMO without MAP failed",
    )?;
    refused(
        side.call(
            nr::VMO_MAP,
            &[reg(no_map), 0, PAGE_SIZE, u64::from(MAP_READ), OFFSET],
        ),
        status::ACCESS_DENIED,
        "a handle without MAP mapped a VMO",
        counter,
    )?;
    Ok([narrow, no_map])
}

/// What `vmo_map` refuses whatever the handle carries: another kind of object,
/// a protection that is not read or read-write, part of a page, a range over
/// another mapping, past the object's end or from inside a page. Returns the
/// channel it made, for the caller to close.
fn check_what_vmo_map_refuses(
    side: &Side,
    vmo: Handle,
    counter: &mut Counter,
) -> Result<[Handle; 2], &'static str> {
    let both = u64::from(MAP_READ | MAP_WRITE);
    let (end, other_end) = side.channel()?;
    let spare = MAPPED + 8 * PAGE_SIZE;
    refused(
        side.call(nr::VMO_MAP, &[reg(end), spare, PAGE_SIZE, both, OFFSET]),
        status::WRONG_TYPE,
        "a channel was mapped as a VMO",
        counter,
    )?;

    // Mappings that cannot be made.
    for (protection, what) in [
        (0, "a mapping with no protection was made"),
        (u64::from(MAP_WRITE), "a write-only mapping was made"),
        (
            both | 0x100,
            "a protection with an unknown bit was accepted",
        ),
    ] {
        refused(
            side.call(
                nr::VMO_MAP,
                &[reg(vmo), spare, PAGE_SIZE, protection, OFFSET],
            ),
            status::INVALID_ARGS,
            what,
            counter,
        )?;
    }
    refused(
        side.call(nr::VMO_MAP, &[reg(vmo), spare, PAGE_SIZE / 2, both, OFFSET]),
        status::INVALID_ARGS,
        "a mapping of part of a page was made",
        counter,
    )?;
    refused(
        side.call(nr::VMO_MAP, &[reg(vmo), MAPPED, PAGE_SIZE, both, OFFSET]),
        status::INVALID_ARGS,
        "a mapping over another mapping was made",
        counter,
    )?;
    side.put_offset(PAGE_SIZE)?;
    refused(
        side.call(nr::VMO_MAP, &[reg(vmo), spare, 2 * PAGE_SIZE, both, OFFSET]),
        status::INVALID_ARGS,
        "a mapping past the end of its VMO was made",
        counter,
    )?;
    side.put_offset(0x10)?;
    refused(
        side.call(nr::VMO_MAP, &[reg(vmo), spare, PAGE_SIZE, both, OFFSET]),
        status::INVALID_ARGS,
        "a mapping from inside a page was made",
        counter,
    )?;

    Ok([end, other_end])
}

/// One of the two processes.
pub(crate) struct Side {
    /// The process, over its own address space.
    pub(crate) process: Arc<Process>,
}

impl Side {
    /// A fresh process with a scratch region mapped.
    pub(crate) fn new() -> Result<Side, &'static str> {
        let process = process::new_for_check().map_err(|_| "could not make a process")?;
        let _ = process
            .space()
            .map_anonymous(SCRATCH, SCRATCH_LEN, VmaFlags::READ_WRITE)
            .map_err(|_| "could not map a scratch region")?;
        Ok(Side { process })
    }

    /// Make a native call with these registers.
    pub(crate) fn call(&self, number: usize, args: &[u64]) -> Result<usize, Errno> {
        let mut registers = [0_u64; 6];
        for (slot, value) in registers.iter_mut().zip(args) {
            *slot = *value;
        }
        let args = SyscallArgs {
            abi: crate::trap::Abi::Native,
            number,
            args: registers,
            ip: 0,
        };
        native::dispatch(&args, Some(&*self.process))
    }

    /// Make a call that returns a handle.
    pub(crate) fn handle(
        &self,
        number: usize,
        args: &[u64],
        what: &'static str,
    ) -> Result<Handle, &'static str> {
        let value = self.call(number, args).map_err(|_| what)?;
        u32::try_from(value).map(Handle).map_err(|_| what)
    }

    /// Put bytes in this process's memory.
    pub(crate) fn put(&self, at: u64, bytes: &[u8]) -> Result<(), &'static str> {
        uaccess::copy_to_user(self.process.space(), at, bytes)
            .map_err(|_| "could not stage user memory")
    }

    /// Read bytes back out of it.
    pub(crate) fn get(&self, at: u64, len: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = vec![0_u8; len];
        uaccess::copy_from_user(self.process.space(), at, &mut out)
            .map_err(|_| "could not read user memory back")?;
        Ok(out)
    }

    /// A `u32` out of it.
    pub(crate) fn get_u32(&self, at: u64) -> Result<u32, &'static str> {
        let bytes = self.get(at, 4)?;
        <[u8; 4]>::try_from(bytes.as_slice())
            .map(u32::from_ne_bytes)
            .map_err(|_| "a short read")
    }

    /// Put handle values at [`HANDLES`].
    fn put_handles(&self, handles: &[Handle]) -> Result<(), &'static str> {
        let words: Vec<u8> = handles.iter().flat_map(|h| h.0.to_ne_bytes()).collect();
        self.put(HANDLES, &words)
    }

    /// Put a VMO offset at [`OFFSET`].
    fn put_offset(&self, offset: u64) -> Result<(), &'static str> {
        self.put(OFFSET, &offset.to_ne_bytes())
    }

    /// Make a channel inside this process, returning both ends.
    fn channel(&self) -> Result<(Handle, Handle), &'static str> {
        let _ = self
            .call(nr::CHANNEL_CREATE, &[PAIR])
            .map_err(|_| "channel_create failed")?;
        Ok((Handle(self.get_u32(PAIR)?), Handle(self.get_u32(PAIR + 4)?)))
    }

    /// Close whatever is left, as a process's exit will.
    pub(crate) fn close_everything(&self) {
        object::dispose(self.process.with_handles(object::HandleTable::clear));
    }
}

/// A handle as a register.
pub(crate) fn reg(handle: Handle) -> u64 {
    u64::from(handle.0)
}

/// The channel end `handle` names in `side`.
fn endpoint(side: &Side, handle: Handle) -> Result<Arc<Endpoint>, &'static str> {
    side.process
        .with_handles(|table| match table.get(handle) {
            Ok((Object::Channel(end), _)) => Some(Arc::clone(end)),
            _ => None,
        })
        .ok_or("a handle did not name a channel end")
}

/// Format what `handle` names in `side`, as a diagnostic would, and require
/// it to begin with `name` (`object::format_check`).
fn formats(side: &Side, handle: Handle, name: &str) -> Result<(), &'static str> {
    let named = side
        .process
        .with_handles(|table| table.get(handle).map(|(object, _)| object.clone()))
        .map_err(|_| "a handle to format named nothing")?;
    let formatted = object::format_check::names(&named, name).map(|_| ());
    object::dispose([named]);
    formatted
}

/// A length as a register.
fn len(bytes: &[u8]) -> u64 {
    bytes.len() as u64
}

/// Require a call to be refused with exactly `wanted`.
fn refused(
    result: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    if result == Err(wanted) {
        counter.refusals += 1;
        Ok(())
    } else {
        Err(what)
    }
}

/// Make a channel in the sender and move one end to the receiver.
///
/// By hand, through the two tables, because that is the only way two
/// processes can share a channel before either can run — and it is exactly
/// what `process::load` followed by `process::start` will do for a real
/// child: put a handle in its table before its first instruction.
fn connect(sender: &Side, receiver: &Side) -> Result<(Handle, Handle), &'static str> {
    let _ = sender
        .call(nr::CHANNEL_CREATE, &[PAIR])
        .map_err(|_| "channel_create failed")?;
    let near = Handle(sender.get_u32(PAIR)?);
    let far = Handle(sender.get_u32(PAIR + 4)?);

    let moved = sender
        .process
        .with_handles(|table| table.take_many(&[far], Rights::NONE))
        .map_err(|_| "could not take the far end out of the sender")?;
    let placed = receiver
        .process
        .with_handles(|table| table.insert_many(moved))
        .map_err(|_| "could not place the far end in the receiver")?;
    let far_there = placed.first().copied().ok_or("no handle was placed")?;

    if sender.call(nr::HANDLE_CLOSE, &[reg(far)]) != Err(status::BAD_HANDLE) {
        return Err("the far end is still in the sender's table");
    }
    Ok((near, far_there))
}

/// Bytes and a VMO handle go from the sender to the receiver, and the handle
/// that arrives names the object that was sent. Returns that handle.
///
/// Verifies: L.object.10, L.object.18
fn check_a_message_carries_a_handle_across(
    sender: &Side,
    receiver: &Side,
    near: Handle,
    far: Handle,
    counter: &mut Counter,
) -> Result<Handle, &'static str> {
    let vmo = sender.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    sender.put_offset(0x10)?;
    sender.put(PAYLOAD, SECRET)?;
    let _ = sender
        .call(nr::VMO_WRITE, &[reg(vmo), PAYLOAD, len(SECRET), OFFSET])
        .map_err(|_| "vmo_write failed")?;

    sender.put(PAYLOAD, PING)?;
    sender.put_handles(&[vmo])?;
    let _ = sender
        .call(
            nr::CHANNEL_WRITE,
            &[reg(near), PAYLOAD, len(PING), HANDLES, 1],
        )
        .map_err(|_| "a message carrying a handle was refused")?;
    counter.moved += 1;
    refused(
        sender.call(nr::VMO_GET_SIZE, &[reg(vmo), SIZE]),
        status::BAD_HANDLE,
        "a handle sent through a channel stayed in the sender's table",
        counter,
    )?;

    check_a_read_that_does_not_fit_takes_nothing(receiver, far, counter)?;

    let _ = receiver
        .call(nr::CHANNEL_READ, &[reg(far), INBOX, 64, HANDLES, 4, ACTUAL])
        .map_err(|_| "a read with room for the message failed")?;
    if receiver.get(INBOX, PING.len())? != PING {
        return Err("the bytes that arrived are not the bytes sent");
    }
    let arrived = Handle(receiver.get_u32(HANDLES)?);
    receiver.put_offset(0x10)?;
    let _ = receiver
        .call(nr::VMO_READ, &[reg(arrived), INBOX, len(SECRET), OFFSET])
        .map_err(|_| "the handle that arrived could not be read through")?;
    if receiver.get(INBOX, SECRET.len())? != SECRET {
        return Err("the handle that arrived names a different object");
    }
    counter.messages += 1;

    refused(
        receiver.call(nr::CHANNEL_READ, &[reg(far), INBOX, 64, HANDLES, 4, ACTUAL]),
        status::SHOULD_WAIT,
        "an empty channel did not say wait",
        counter,
    )?;
    refused(
        receiver.call(nr::VMO_READ, &[reg(far), INBOX, 1, OFFSET]),
        status::WRONG_TYPE,
        "a channel was read as a VMO",
        counter,
    )?;
    Ok(arrived)
}

/// Too little room for the bytes, then for the handle: both refused, the
/// sizes reported, and the message still there.
///
/// Verifies: L.object.14
fn check_a_read_that_does_not_fit_takes_nothing(
    receiver: &Side,
    far: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    refused(
        receiver.call(nr::CHANNEL_READ, &[reg(far), INBOX, 3, HANDLES, 1, ACTUAL]),
        status::BUFFER_TOO_SMALL,
        "a read into three bytes took a four-byte message",
        counter,
    )?;
    if (receiver.get_u32(ACTUAL)?, receiver.get_u32(ACTUAL + 4)?) != (4, 1) {
        return Err("a read that did not fit reported the wrong sizes");
    }
    refused(
        receiver.call(nr::CHANNEL_READ, &[reg(far), INBOX, 64, HANDLES, 0, ACTUAL]),
        status::BUFFER_TOO_SMALL,
        "a read with no room for a handle took a message carrying one",
        counter,
    )
}

/// A narrower duplicate cannot do what it lacks, and cannot get it back.
fn check_rights_only_shrink(
    side: &Side,
    vmo: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let read_only = u64::from((Rights::DUPLICATE | Rights::READ).0);
    let narrow = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(vmo), read_only],
        "duplicating with fewer rights failed",
    )?;
    refused(
        side.call(nr::VMO_WRITE, &[reg(narrow), INBOX, 1, OFFSET]),
        status::ACCESS_DENIED,
        "a handle without WRITE wrote",
        counter,
    )?;
    let _ = side
        .call(nr::VMO_READ, &[reg(narrow), INBOX, 1, OFFSET])
        .map_err(|_| "a read-only handle could not read")?;
    refused(
        side.call(
            nr::HANDLE_DUPLICATE,
            &[reg(narrow), u64::from(Rights::WRITE.0)],
        ),
        status::ACCESS_DENIED,
        "a duplicate gained a right its original lacked",
        counter,
    )?;
    refused(
        side.call(nr::HANDLE_DUPLICATE, &[reg(narrow), 0x100]),
        status::INVALID_ARGS,
        "a request for an undefined right was accepted",
        counter,
    )?;

    let replaced = side.handle(
        nr::HANDLE_REPLACE,
        &[reg(narrow), u64::from(Rights::READ.0)],
        "handle_replace failed",
    )?;
    refused(
        side.call(nr::HANDLE_CLOSE, &[reg(narrow)]),
        status::BAD_HANDLE,
        "a replaced handle still resolves",
        counter,
    )?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(replaced)])
        .map_err(|_| "closing a replacement failed")?;
    refused(
        side.call(nr::HANDLE_CLOSE, &[reg(replaced)]),
        status::BAD_HANDLE,
        "a handle was closed twice",
        counter,
    )
}

/// A send that fails keeps every handle it named, under the same number.
///
/// Verifies: L.object.11
fn check_a_refused_send_keeps_its_handles(
    sender: &Side,
    near: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let kept = sender.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    sender.put_handles(&[kept, Handle(0xDEAD_B001)])?;
    refused(
        sender.call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 1, HANDLES, 2]),
        status::BAD_HANDLE,
        "a send naming a handle that does not exist was accepted",
        counter,
    )?;
    let _ = sender
        .call(nr::VMO_GET_SIZE, &[reg(kept), SIZE])
        .map_err(|_| "a refused send took a good handle with it")?;

    sender.put_handles(&[near])?;
    refused(
        sender.call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 1, HANDLES, 1]),
        status::INVALID_ARGS,
        "a channel end was sent through itself",
        counter,
    )?;
    refused(
        sender.call(
            nr::CHANNEL_WRITE,
            &[reg(near), PAYLOAD, CHANNEL_MAX_BYTES as u64 + 1, HANDLES, 0],
        ),
        status::TOO_BIG,
        "a message over the size limit was accepted",
        counter,
    )?;
    let _ = sender
        .call(nr::HANDLE_CLOSE, &[reg(kept)])
        .map_err(|_| "closing a kept handle failed")?;
    Ok(())
}

/// Writes to a reader that is not reading are told to wait, not queued
/// without end, and draining makes room again.
///
/// Verifies: L.object.13
fn check_a_full_channel_says_wait(
    sender: &Side,
    near: Handle,
    receiver: &Side,
    far: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let empty = [reg(near), PAYLOAD, 0, HANDLES, 0];
    let mut queued = 0_u32;
    loop {
        match sender.call(nr::CHANNEL_WRITE, &empty) {
            Ok(_) => queued += 1,
            Err(error) if error == status::SHOULD_WAIT => break,
            Err(_) => return Err("filling a channel failed for a reason other than full"),
        }
        if queued > 4096 {
            return Err("a channel's queue has no bound");
        }
    }
    counter.refusals += 1;
    let writer = endpoint(sender, near)?;
    if writer.peer_has_room() || writer.signals().intersects(Signals::WRITABLE) {
        return Err("the writer of a full channel was told it could write");
    }

    let drain = [reg(far), INBOX, 0, HANDLES, 0, ACTUAL];
    for _ in 0..queued {
        let _ = receiver
            .call(nr::CHANNEL_READ, &drain)
            .map_err(|_| "draining a full channel failed")?;
    }
    if !writer.peer_has_room() {
        return Err("the writer of a drained channel was told it had no room");
    }
    drop(writer);
    let _ = sender
        .call(nr::CHANNEL_WRITE, &empty)
        .map_err(|_| "a drained channel still refused a write")?;
    let _ = receiver
        .call(nr::CHANNEL_READ, &drain)
        .map_err(|_| "the write after draining did not arrive")?;
    Ok(())
}

/// Closing the reader ends the channel for the writer, and frees a VMO that
/// was still queued, unread, inside it. `run`'s frame count is what says the
/// VMO's page came back.
///
/// Closed while another processor drains disposed objects, and the writer
/// told at once: a close used to queue the end behind that drain, and returned
/// with the channel still open. Stage 10's block ring check quiesced a device
/// the moment its driver's channel closed and was refused (FX-1004).
///
/// Verifies: L.object.4, L.object.6, L.object.17, L.object.20, H.OBJ.16
fn check_a_closed_peer_frees_what_was_queued(
    sender: &Side,
    near: Handle,
    receiver: &Side,
    far: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let held = sender.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    sender.put_offset(0)?;
    sender.put(PAYLOAD, b"x")?;
    let _ = sender
        .call(nr::VMO_WRITE, &[reg(held), PAYLOAD, 1, OFFSET])
        .map_err(|_| "vmo_write failed")?;
    sender.put_handles(&[held])?;
    let _ = sender
        .call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 0, HANDLES, 1])
        .map_err(|_| "queueing a VMO failed")?;

    object::as_if_draining_elsewhere(|| {
        let _ = receiver
            .call(nr::HANDLE_CLOSE, &[reg(far)])
            .map_err(|_| "closing the far end failed")?;
        refused(
            sender.call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 0, HANDLES, 0]),
            status::PEER_CLOSED,
            "a write to a closed channel was accepted",
            counter,
        )?;
        refused(
            sender.call(
                nr::CHANNEL_READ,
                &[reg(near), INBOX, 64, HANDLES, 4, ACTUAL],
            ),
            status::PEER_CLOSED,
            "an empty channel with a closed peer said wait instead of closed",
            counter,
        )
    })?;
    let _ = sender
        .call(nr::HANDLE_CLOSE, &[reg(near)])
        .map_err(|_| "closing the near end failed")?;
    Ok(())
}

/// Two channels cannot be made to hold each other, and the send that tries
/// keeps its handle.
///
/// The leak this prevents is invisible to every other check: two endpoints
/// each queued in the other's inbox outlive every handle to both, with
/// whatever they hold. So a VMO with a committed page rides in one of the
/// queues, and if the refusal ever stops happening, `run`'s frame count is
/// what fails.
///
/// Verifies: L.object.24
fn check_a_cycle_of_channels_is_refused(
    side: &Side,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let (first, first_far) = side.channel()?;
    let (second, second_far) = side.channel()?;

    let held = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    side.put_offset(0)?;
    side.put(PAYLOAD, b"x")?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(held), PAYLOAD, 1, OFFSET])
        .map_err(|_| "vmo_write failed")?;
    side.put_handles(&[held])?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(second), PAYLOAD, 0, HANDLES, 1])
        .map_err(|_| "queueing a VMO failed")?;

    // One edge, which is fine: `second_far`, holding the VMO, is queued in
    // `first_far`.
    side.put_handles(&[second_far])?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(first), PAYLOAD, 0, HANDLES, 1])
        .map_err(|_| "queueing one endpoint inside another was refused")?;

    // The edge back would close the loop: `first_far` into `second_far`.
    side.put_handles(&[first_far])?;
    refused(
        side.call(nr::CHANNEL_WRITE, &[reg(second), PAYLOAD, 0, HANDLES, 1]),
        status::INVALID_ARGS,
        "a send closing a cycle of two channels was accepted",
        counter,
    )?;
    // And the shortest loop: an end sent into its own inbox by its peer.
    refused(
        side.call(nr::CHANNEL_WRITE, &[reg(first), PAYLOAD, 0, HANDLES, 1]),
        status::INVALID_ARGS,
        "a channel end was sent into its own inbox",
        counter,
    )?;

    // `first_far` was kept by both refusals. Closing it frees `second_far`
    // queued in it, and the VMO queued in that.
    for end in [first, second, first_far] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(end)])
            .map_err(|_| "closing a channel end after the cycle check failed")?;
    }
    Ok(())
}

/// The process and channel end the waker writes into.
static WAKER: SpinLock<Option<(Arc<Process>, Handle)>> = SpinLock::new(None);

/// When [`write_after_a_delay`] last began its write, on the counter; zero
/// until it does. What a woken wait is judged against: it must come back
/// after this, not merely some time after it began.
static WRITTEN_AT: AtomicU64 = AtomicU64::new(0);

/// A peer that writes three messages and closes has all three read, in
/// order, before a read is told it closed: the fourth.
///
/// The rule the net ring's REFUSED was lost against under WHPX (FX-1151,
/// F-54). The write and close landing inside a read, which is how it was
/// lost, cannot be scheduled on demand; F-54 argues it closed, and this
/// holds the rule the argument rests on.
///
/// Verifies: L.object.19
fn check_messages_before_the_close_are_read_first() -> Result<(), &'static str> {
    const SENT: [&[u8]; 3] = [b"one", b"two", b"three"];
    let (reader, writer) =
        Endpoint::pair().map_err(|_| "no memory for the last-message check's channel")?;
    for bytes in SENT {
        writer
            .write(bytes.to_vec(), 0, || {
                Ok::<Vec<object::Transfer>, core::convert::Infallible>(Vec::new())
            })
            .map_err(|_| "the last-message check's peer could not write")?;
    }
    drop(writer);
    for bytes in SENT {
        match reader.read(CHANNEL_MAX_BYTES, 0, false) {
            Ok(message) if message.bytes == bytes => {}
            Err(ReadError::PeerClosed) => {
                return Err("a read told the peer closed with its messages still queued");
            }
            _ => return Err("a closed peer's messages were not read back in order"),
        }
    }
    match reader.read(CHANNEL_MAX_BYTES, 0, false) {
        Err(ReadError::PeerClosed) => Ok(()),
        _ => Err("an empty channel whose peer closed did not say so"),
    }
}

/// A kernel thread that writes one empty message after a delay, so a wait
/// has something other than its deadline to end it.
fn write_after_a_delay(_: usize) {
    crate::sched::sleep_for(WAKE_AFTER_NANOS);
    let taken = WAKER.lock().take();
    if let Some((process, end)) = taken {
        WRITTEN_AT.store(crate::timer::now_nanos(), Ordering::Release);
        let args = SyscallArgs {
            abi: crate::trap::Abi::Native,
            number: nr::CHANNEL_WRITE,
            args: [reg(end), 0, 0, 0, 0, 0],
            ip: 0,
        };
        let _ = native::dispatch(&args, Some(&*process));
    }
}

/// Whether a wait that came back at `returned` did so after the waker began
/// its write. It used to be judged by how long the wait took from a time
/// taken after the waker was spawned, and a check task held up for more
/// than half the delay between the two -- by a host that stopped its
/// processor, under the coverage plugin -- saw the write land early and
/// failed, the wait being right (FX-0901, 2026-09-26).
fn came_back_after_the_write(returned: u64) -> bool {
    let written = WRITTEN_AT.load(Ordering::Acquire);
    written != 0 && returned >= written
}

/// A deadline `nanos` from now, staged at [`DEADLINE`].
fn stage_deadline(side: &Side, nanos: u64) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(nanos);
    side.put(DEADLINE, &deadline.to_ne_bytes())
}

/// A wait ends when its signal is asserted, at once if it already is, and at
/// its deadline if it never is.
///
/// The woken case is the one that matters, and the one easiest to fake: a
/// wait that only ever polled until its deadline would pass every other part
/// of this. So the deadline is two minutes, the message arrives after twenty
/// milliseconds, and the wait has to come back in between.
///
/// Verifies: L.object.3, L.object.22, H.OBJ.11
fn check_a_wait_is_woken_by_what_it_waits_for(counter: &mut Counter) -> Result<(), &'static str> {
    let side = Side::new()?;
    let (near, far) = side.channel()?;
    let readable = u64::from((Signals::READABLE | Signals::PEER_CLOSED).0);
    let wait = |signals: u64| {
        side.call(
            nr::OBJECT_WAIT_ONE,
            &[reg(far), signals, DEADLINE, OBSERVED],
        )
    };

    stage_deadline(&side, 0)?;
    refused(
        wait(readable),
        status::TIMED_OUT,
        "a wait on an empty channel said it was ready",
        counter,
    )?;

    WRITTEN_AT.store(0, Ordering::Release);
    *WAKER.lock() = Some((Arc::clone(&side.process), near));
    let _waker = crate::sched::spawn(
        "native waker",
        write_after_a_delay,
        0,
        ferrix_sched::NICE_0_WEIGHT,
    )
    .map_err(|_| "could not start the waker")?;
    stage_deadline(&side, PATIENCE_NANOS)?;
    let woke = wait(readable);
    let returned = crate::timer::now_nanos();
    *WAKER.lock() = None;
    if woke != Ok(0) {
        return Err("a wait was not woken by the message it waited for");
    }
    if !came_back_after_the_write(returned) {
        return Err("a wait returned before anything had been written");
    }
    if !Signals(side.get_u32(OBSERVED)?).intersects(Signals::READABLE) {
        return Err("a woken wait did not report the channel readable");
    }
    counter.woken += 1;

    let _ = side
        .call(nr::CHANNEL_READ, &[reg(far), INBOX, 0, HANDLES, 0, ACTUAL])
        .map_err(|_| "reading the message that woke a wait failed")?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(near)])
        .map_err(|_| "closing a channel end failed")?;
    stage_deadline(&side, PATIENCE_NANOS)?;
    let _ = wait(u64::from(Signals::PEER_CLOSED.0))
        .map_err(|_| "a wait for a peer that had already closed did not return")?;

    let vmo = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    refused(
        side.call(
            nr::OBJECT_WAIT_ONE,
            &[reg(vmo), readable, DEADLINE, OBSERVED],
        ),
        status::ACCESS_DENIED,
        "a handle without WAIT was waited on",
        counter,
    )?;
    refused(
        wait(1 << 5),
        status::INVALID_ARGS,
        "a wait for a signal that does not exist was accepted",
        counter,
    )?;
    side.close_everything();
    Ok(())
}

/// The job a handle in `side` names.
fn job_of(side: &Side, handle: Handle) -> Result<Arc<Job>, &'static str> {
    side.process.with_handles(|table| match table.get(handle) {
        Ok((Object::Job(job), _)) => Ok(Arc::clone(job)),
        _ => Err("a handle that should name a job does not"),
    })
}

/// Load a spinning program into the job `handle` names, and start it.
fn run_in(side: &Side, handle: Handle, process: &Arc<Process>) -> Result<Arc<Task>, &'static str> {
    job_of(side, handle)?
        .adopt(process)
        .map_err(|_| "a live job refused a process")?;
    process::start(process).map_err(|_| "a program in a job could not be started")
}

/// A process ended by a job kill: it reports the kill's status, and its task
/// actually stops, for the reason `syscall::check`'s own kill check gives.
fn ended_by_the_kill(process: &Process, task: &Task) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if process.wait_for_exit(deadline) != Some(KILLED_STATUS) {
        return Err("a process in a killed job did not report the kill's status");
    }
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err("a process in a killed job kept running");
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// Killing a job ends every process in it and beneath it, and nothing above
/// or beside it; a killed job stays killed.
///
/// A tree of three jobs, root, middle and leaf, with a program in each that
/// is blocked in a native wait nobody will ever satisfy, and a fourth program
/// in no job at all. Killing
/// the middle job must end the middle and leaf programs and leave the root's
/// running; killing the root must end that one; the bystander must finish
/// with its own status.
/// A copy of the native program as a receiver with nobody to talk to.
///
/// It blocks in `object_wait_one` with no deadline, so only a kill ends it,
/// which is also the case `process::kill` has to get right for a waiting
/// task: the wait must notice. A program told to spin a fixed number of
/// rounds is not a substitute -- on four emulated ARMv7-A processors
/// `u32::MAX` rounds took seconds, and the root job's program finished and
/// exited before the kill it was meant to survive had even been sent.
///
/// The far end is returned and must be held until the check is done, or the
/// wait sees `PEER_CLOSED` and the program ends by itself.
fn waiting_program() -> Result<(Arc<Process>, Arc<Endpoint>), &'static str> {
    let program = native_program(b'r')?;
    let (near, far) = Endpoint::pair().map_err(|_| "could not make a channel")?;
    let placed = program
        .with_handles(|table| table.insert(Object::Channel(near), Rights::CHANNEL))
        .map_err(|_| "no room for a bootstrap handle")?;
    if placed != BOOTSTRAP {
        return Err("a fresh process's first handle is not the one its program was built for");
    }
    Ok((program, far))
}

/// Verifies: L.object.75, L.object.76, H.OBJ.9
fn check_a_job_kill_takes_down_a_process_tree(counter: &mut Counter) -> Result<(), &'static str> {
    if arch::USER_SPIN_PROGRAM.is_empty() {
        return Ok(());
    }
    let side = Side::new()?;
    let job = Job::new_root().map_err(|_| "no memory for a job")?;
    let root = side
        .process
        .with_handles(|table| table.insert(Object::Job(job), Rights::JOB))
        .map_err(|_| "no room for a job handle")?;
    let middle = side.handle(
        nr::JOB_CREATE,
        &[reg(root)],
        "job_create under a root failed",
    )?;
    let leaf = side.handle(
        nr::JOB_CREATE,
        &[reg(middle)],
        "job_create under a child failed",
    )?;

    let (top, _top_far) = waiting_program()?;
    let (inner, _inner_far) = waiting_program()?;
    let (deepest, _deepest_far) = waiting_program()?;
    let bystander = spinner(b'b', 1_000_000, 44)?;
    let top_task = run_in(&side, root, &top)?;
    let inner_task = run_in(&side, middle, &inner)?;
    let deepest_task = run_in(&side, leaf, &deepest)?;
    // A spinning member as well, alone on whatever processor it lands on.
    // Nothing but the kill's interrupt reaches a task that never enters the
    // kernel, so without it this program spins to the end of its rounds and
    // reports its own status, and the check fails every time rather than now
    // and then.
    let spinning = spinner(b's', u32::MAX, 6)?;
    let spinning_task = run_in(&side, middle, &spinning)?;
    let _bystander_task =
        process::start(&bystander).map_err(|_| "the bystander could not be started")?;
    crate::sched::sleep_for(KILL_AFTER_NANOS);

    // A registration on the middle job, to be fired by the kill below.
    let watching = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(KEY, &31_u64.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ASYNC,
            &[
                reg(middle),
                reg(watching),
                u64::from(Signals::TERMINATED.0),
                KEY,
            ],
        )
        .map_err(|_| "object_wait_async on a job failed")?;
    let _ = side
        .call(nr::JOB_KILL, &[reg(middle)])
        .map_err(|_| "job_kill failed")?;
    stage_deadline(&side, PATIENCE_NANOS)?;
    let _ = side
        .call(nr::PORT_WAIT, &[reg(watching), DEADLINE, PACKET_AT])
        .map_err(|_| "killing a job did not fire the registration watching it")?;
    let (key, kind, signals, _, _) = read_packet(&side)?;
    if key != 31 || kind != PACKET_SIGNAL || !Signals(signals).intersects(Signals::TERMINATED) {
        return Err("a killed job's signal packet did not say what fired it");
    }
    counter.packets += 1;
    ended_by_the_kill(&inner, &inner_task)?;
    ended_by_the_kill(&deepest, &deepest_task)?;
    ended_by_the_kill(&spinning, &spinning_task)?;
    if top.is_terminated() {
        return Err("killing a child job ended a process in its parent");
    }
    counter.killed += 3;

    refused(
        side.call(nr::JOB_CREATE, &[reg(middle)]),
        status::BAD_STATE,
        "a killed job made a child",
        counter,
    )?;
    let late = native_program(b'r')?;
    if job_of(&side, leaf)?.adopt(&late).is_ok() {
        return Err("a job beneath a killed one took a new process");
    }
    stage_deadline(&side, PATIENCE_NANOS)?;
    let terminated = u64::from(Signals::TERMINATED.0);
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[reg(middle), terminated, DEADLINE, OBSERVED],
        )
        .map_err(|_| "a killed job did not say it was terminated")?;

    let _ = side
        .call(nr::JOB_KILL, &[reg(root)])
        .map_err(|_| "job_kill of the root failed")?;
    ended_by_the_kill(&top, &top_task)?;
    counter.killed += 1;

    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if bystander.wait_for_exit(deadline) != Some(44) {
        return Err("a process outside the killed jobs did not finish with its own status");
    }
    side.close_everything();
    Ok(())
}

/// Where the eight patchable bytes of [`arch::USER_NATIVE_PROGRAM`] begin.
const NATIVE_TAIL: usize = 28;

/// The handle a fresh process's first handle gets: slot 0, generation 1.
///
/// The program cannot be told its bootstrap handle after it is loaded -- its
/// text is not writable -- so it is built for this value, and the check
/// asserts the value when it places the handle. A change to how the table
/// encodes handles fails there, by name, rather than as a program talking to
/// a handle it does not hold.
const BOOTSTRAP: Handle = Handle(1);

/// Load a copy of the native program playing `role`.
fn native_program(role: u8) -> Result<Arc<Process>, &'static str> {
    let mut program = arch::USER_NATIVE_PROGRAM.to_vec();
    let tail = program
        .get_mut(NATIVE_TAIL..NATIVE_TAIL + 8)
        .ok_or("the native program is shorter than its own layout")?;
    if tail.first() != Some(&b'?') || tail.get(1) != Some(&b'\n') {
        return Err("the native program's tail is not where its layout says");
    }
    let [h0, h1, h2, h3] = BOOTSTRAP.0.to_le_bytes();
    tail.copy_from_slice(&[role, b'\n', 0, 0, h0, h1, h2, h3]);

    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        &program,
    );
    process::load(
        &file,
        &[b"/native"],
        &[],
        [0x39; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the native program could not be loaded")
}

/// What the device checks measured, for the boot log.
#[derive(Debug)]
pub(crate) struct DeviceReport {
    /// Apertures mapped into a process, and reached from a forked child.
    pub(crate) mapped: u32,
    /// Prefetchable apertures mapped write-combining, and found so in the
    /// page table.
    pub(crate) combined: u32,
    /// Interrupts delivered to an object, waited on and acknowledged.
    pub(crate) interrupts: u32,
    /// VMO pages pinned for a device and found at their device addresses.
    pub(crate) pinned: u32,
    /// Calls refused with exactly the status they had to be refused with.
    pub(crate) refusals: u32,
    /// Interrupt deliveries made to a waiting task.
    pub(crate) delivered: u32,
    /// Those whose wait the delivery's wake ended, rather than its recheck.
    pub(crate) wakes: u32,
    /// The longest any delivery took to become a returned wait, in
    /// nanoseconds: the host's latency as much as the kernel's, so reported
    /// and never judged.
    pub(crate) slowest_wake: u64,
    /// Edge-triggered MSI-X vectors found unmasked through their deliveries,
    /// and masked by the delivery past `interrupt::STORM_BOUND`.
    pub(crate) coalesced: u32,
    /// What those bounded deliveries took, in nanoseconds, all of it charged
    /// to the task that ran them: reported, never judged.
    pub(crate) storm_nanos: u64,
}

/// The device objects: I/O mappings and interrupts, minted from the device
/// nodes stage 10 publishes.
///
/// A separate entry point from [`run`], and called later, because it needs
/// those nodes. It first ran inside [`run`], before they were published,
/// found none, and passed on every machine having checked nothing -- which
/// the boot log's `0 aperture mapped` said and nothing else did. So the
/// counts are printed, and on a machine with a whole-page aperture a count of
/// zero is a failure rather than a skip.
pub(crate) fn run_devices() -> Result<DeviceReport, &'static str> {
    let mut counter = Counter::default();
    check_a_device_gives_exactly_its_own_memory(&mut counter)?;
    check_write_combining(&mut counter)?;
    check_an_interrupt_is_held_until_acknowledged(&mut counter)?;
    check_a_pin_gives_a_device_exactly_its_pages(&mut counter)?;
    let has_whole_page = device::devices().iter().any(|node| {
        node.apertures()
            .iter()
            .any(|aperture| aperture.whole_pages())
    });
    if has_whole_page && counter.mapped == 0 {
        return Err("a machine with a whole-page device aperture mapped none");
    }
    let has_vector = device::devices()
        .iter()
        .any(|node| node.vector(0).is_some());
    if has_vector && counter.interrupts == 0 {
        return Err("a machine with a device vector held no interrupt");
    }
    let has_pci = device::devices()
        .iter()
        .any(|node| matches!(node.location(), device::Location::Pci(_)));
    if has_pci && counter.pinned == 0 {
        return Err("a machine with a PCI function pinned no page for it");
    }
    Ok(DeviceReport {
        mapped: counter.mapped,
        combined: counter.combined,
        interrupts: counter.interrupts,
        pinned: counter.pinned,
        refusals: counter.refusals,
        delivered: counter.delivered,
        wakes: counter.wakes,
        slowest_wake: counter.slowest_wake,
        coalesced: counter.coalesced,
        storm_nanos: counter.storm_nanos,
    })
}

/// A device is given exactly the VMO pages pinned for it, at the addresses its
/// domain chose, holding what the VMO holds, and a pin is refused whatever the
/// rules refuse.
///
/// On a translated domain the pages must also be unreachable once the pin is
/// closed. On an untranslated one they stay held, which the console says.
///
/// Verifies: L.object.45, L.object.46, L.iommu.24
fn check_a_pin_gives_a_device_exactly_its_pages(counter: &mut Counter) -> Result<(), &'static str> {
    let Some(node) = device::devices()
        .iter()
        .find(|node| matches!(node.location(), device::Location::Pci(_)))
        .cloned()
    else {
        return Ok(());
    };
    let side = Side::new()?;
    let handle = device_handle(&side, &node)?;
    formats(&side, handle, "Device(")?;
    let vmo = side.handle(
        nr::VMO_CREATE,
        &[2 * PAGE_SIZE],
        "vmo_create for a pin failed",
    )?;

    check_pin_refusals(&side, handle, vmo, counter)?;
    check_a_coherent_pin_of_a_snooping_device_changes_nothing(&side, handle)?;

    side.put(PAYLOAD, SECRET)?;
    side.put_offset(PAGE_SIZE)?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), PAYLOAD, len(SECRET), OFFSET])
        .map_err(|_| "vmo_write before a pin failed")?;
    let pin = side.handle(
        nr::VMO_PIN,
        &[reg(handle), reg(vmo), 0, 2 * PAGE_SIZE, 0],
        "vmo_pin of a VMO for its own device failed",
    )?;
    formats(&side, pin, "Pin(Pin {")?;
    check_a_pin_stays_where_it_was_made(&side, pin, counter)?;
    let pages = side
        .call(nr::VMO_PIN_ADDRESSES, &[reg(pin), PINNED_AT, 2])
        .map_err(|_| "vmo_pin_addresses failed")?;
    if pages != 2 {
        return Err("a pin of two pages said it held another number");
    }
    let bytes = side.get(PINNED_AT, 16)?;
    let addresses: Vec<u64> = bytes
        .chunks_exact(8)
        .filter_map(|word| <[u8; 8]>::try_from(word).ok())
        .map(u64::from_ne_bytes)
        .collect();
    let domain = node
        .domain()
        .map_err(|_| "no memory for a device's domain")?;
    let Some(second) = addresses
        .get(1)
        .and_then(|&address| domain.resolve(address))
    else {
        return Err("a pinned page's device address led nowhere");
    };
    // SAFETY: (FRAME) `second` is the frame holding the VMO's second page, held by the
    // pin for as long as this runs, and the direct map covers every frame; the
    // read is shorter than a page.
    let seen =
        unsafe { core::slice::from_raw_parts(mm::direct_map(second) as *const u8, SECRET.len()) };
    if seen != SECRET {
        return Err("a pinned page's device address held something other than the VMO's page");
    }
    if domain.translated() {
        refused(
            side.call(
                nr::VMO_PIN,
                &[reg(handle), reg(vmo), PAGE_SIZE, PAGE_SIZE, 0],
            ),
            status::ALREADY_BOUND,
            "a page already pinned into a translated domain was pinned again",
            counter,
        )?;
    }

    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(pin)])
        .map_err(|_| "closing a pin failed")?;
    if domain.translated()
        && addresses
            .iter()
            .any(|&address| domain.resolve(address).is_some())
    {
        return Err("a closed pin's pages were still reachable through its domain");
    }
    counter.pinned += 2;
    side.close_everything();
    Ok(())
}

/// Require a pin to stay in the process that made it: a channel write carrying
/// it and a duplicate of it are both refused, because its rights carry neither
/// `TRANSFER` nor `DUPLICATE`.
fn check_a_pin_stays_where_it_was_made(
    side: &Side,
    pin: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let (near, _far) = side.channel()?;
    side.put_handles(&[pin])?;
    refused(
        side.call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 0, HANDLES, 1]),
        status::ACCESS_DENIED,
        "a pin was sent over a channel",
        counter,
    )?;
    refused(
        side.call(nr::HANDLE_DUPLICATE, &[reg(pin), u64::from(Rights::READ.0)]),
        status::ACCESS_DENIED,
        "a pin was duplicated",
        counter,
    )
}

/// Require `vmo_pin` to refuse, with exactly the status the rules give, a
/// range off a page boundary, one past the VMO's end, an unknown option, an
/// empty range, and a writable pin through a read-only VMO handle.
fn check_pin_refusals(
    side: &Side,
    handle: Handle,
    vmo: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    for (args, what) in [
        ([1, PAGE_SIZE, 0], "a pin not on a page boundary was taken"),
        (
            [0, 3 * PAGE_SIZE, 0],
            "a pin past the end of its VMO was taken",
        ),
        // 1 PIN_READ_ONLY, 2 PIN_COHERENT, 4 PIN_CONTIGUOUS: 8 is unknown.
        ([0, PAGE_SIZE, 8], "a pin with an unknown option was taken"),
        ([0, 0, 0], "an empty pin was taken"),
    ] {
        let [offset, length, options] = args;
        refused(
            side.call(
                nr::VMO_PIN,
                &[reg(handle), reg(vmo), offset, length, options],
            ),
            status::INVALID_ARGS,
            what,
            counter,
        )?;
    }
    let reader = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(vmo), u64::from(Rights::READ.0)],
        "handle_duplicate of a VMO failed",
    )?;
    refused(
        side.call(nr::VMO_PIN, &[reg(handle), reg(reader), 0, PAGE_SIZE, 0]),
        status::ACCESS_DENIED,
        "a writable pin was taken through a read-only VMO handle",
        counter,
    )?;
    Ok(())
}

/// `PIN_COHERENT` on a device that snoops the caches -- every PCI function
/// QEMU gives -- changes nothing: a pin of part of a VMO is taken, where one
/// for a device that does not snoop must cover it all, and the VMO is still
/// one the kernel copies into, since its mappings stay cached.
fn check_a_coherent_pin_of_a_snooping_device_changes_nothing(
    side: &Side,
    handle: Handle,
) -> Result<(), &'static str> {
    let vmo = side.handle(
        nr::VMO_CREATE,
        &[2 * PAGE_SIZE],
        "vmo_create for a coherent pin failed",
    )?;
    let coherent = ferrix_native_abi::types::PIN_COHERENT;
    let pin = side.handle(
        nr::VMO_PIN,
        &[reg(handle), reg(vmo), 0, PAGE_SIZE, coherent],
        "a coherent pin of part of a VMO for a device that snoops was refused",
    )?;
    side.put(PAYLOAD, SECRET)?;
    side.put_offset(0)?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), PAYLOAD, len(SECRET), OFFSET])
        .map_err(|_| "a coherent pin for a device that snoops stopped vmo_write")?;
    for held in [pin, vmo] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(held)])
            .map_err(|_| "closing a coherent pin's handles failed")?;
    }
    Ok(())
}

/// Give `side` a handle to `node`, as `devmgr` will give one to a driver.
pub(crate) fn device_handle(side: &Side, node: &Arc<DeviceNode>) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Device(Arc::clone(node)), Rights::DEVICE))
        .map_err(|_| "no room for a device handle")
}

/// Stage a spec for `len` bytes at `phys` at [`SPEC`].
fn stage_spec(side: &Side, phys: u64, len: u64) -> Result<(), &'static str> {
    side.put(SPEC, &phys.to_ne_bytes())?;
    side.put(SPEC + 8, &len.to_ne_bytes())
}

/// Whether `space` translates `at` to `phys`, after faulting it in.
///
/// The page is faulted in and its translation read back, and nothing reads
/// the device memory itself: the direct map covers RAM, not registers, and a
/// read of a real device register can have side effects.
fn reaches(
    space: &crate::user::space::AddressSpace,
    at: u64,
    phys: u64,
) -> Result<bool, &'static str> {
    space
        .fault(at, Access::READ)
        .map_err(|_| "a device page could not be faulted in")?;
    Ok(mm::translate_in(space.root_table(), at) == Some(phys))
}

/// A device handle yields a mapping of its own aperture and of nothing beyond
/// it, the mapping lands on the device's physical pages, and a forked child
/// reaches the same pages rather than a copy of them.
///
/// Every machine has a whole-page aperture except where firmware found none,
/// in which case the mapping half reports nothing mapped. The refusal of a
/// sub-page aperture runs wherever one exists: the virtio-mmio transports on
/// ARMv7-A.
///
/// Verifies: L.object.43, L.object.44, L.user.68
fn check_a_device_gives_exactly_its_own_memory(counter: &mut Counter) -> Result<(), &'static str> {
    let side = Side::new()?;

    let sub_page = device::devices().iter().find_map(|node| {
        node.apertures()
            .iter()
            .find(|aperture| !aperture.whole_pages())
            .map(|aperture| (Arc::clone(node), *aperture))
    });
    if let Some((node, aperture)) = sub_page {
        let handle = device_handle(&side, &node)?;
        stage_spec(&side, aperture.phys(), aperture.len())?;
        refused(
            side.call(nr::IO_MAPPING_CREATE, &[reg(handle), SPEC]),
            status::INVALID_ARGS,
            "an aperture smaller than a page was mapped, taking its neighbours with it",
            counter,
        )?;
    }

    let Some((node, aperture)) = device::devices().iter().find_map(|node| {
        node.apertures()
            .iter()
            .find(|aperture| aperture.whole_pages())
            .map(|aperture| (Arc::clone(node), *aperture))
    }) else {
        side.close_everything();
        return Ok(());
    };
    let handle = device_handle(&side, &node)?;

    stage_spec(&side, aperture.phys(), aperture.len() + 1)?;
    refused(
        side.call(nr::IO_MAPPING_CREATE, &[reg(handle), SPEC]),
        status::ACCESS_DENIED,
        "a range one byte past a device's aperture was granted",
        counter,
    )?;

    stage_spec(&side, aperture.phys(), PAGE_SIZE / 2)?;
    refused(
        side.call(nr::IO_MAPPING_CREATE, &[reg(handle), SPEC]),
        status::INVALID_ARGS,
        "half a page of a whole-page aperture was mapped, taking the rest with it",
        counter,
    )?;

    stage_spec(&side, aperture.phys(), aperture.len())?;
    let mapping = side.handle(
        nr::IO_MAPPING_CREATE,
        &[reg(handle), SPEC],
        "io_mapping_create of a device's own aperture failed",
    )?;
    formats(&side, mapping, "IoMapping(IoMapping {")?;
    let at = side
        .call(nr::IO_MAPPING_MAP, &[reg(mapping), 0])
        .map_err(|_| "io_mapping_map failed")? as u64;
    if !reaches(side.process.space(), at, aperture.phys())? {
        return Err("a mapped aperture does not translate to the device's own memory");
    }
    let child = side
        .process
        .space()
        .fork()
        .map_err(|_| "could not fork an address space holding a device mapping")?;
    if !reaches(&child, at, aperture.phys())? {
        return Err("a forked child does not reach the same device memory as its parent");
    }
    drop(child);

    // One driver per device: a mapping handle carries no DUPLICATE, so it
    // cannot be copied, only moved or narrowed.
    refused(
        side.call(
            nr::HANDLE_DUPLICATE,
            &[reg(mapping), u64::from(Rights::TRANSFER.0)],
        ),
        status::ACCESS_DENIED,
        "a mapping handle was duplicated",
        counter,
    )?;
    let narrow = side.handle(
        nr::HANDLE_REPLACE,
        &[reg(mapping), u64::from(Rights::TRANSFER.0)],
        "narrowing a mapping handle to TRANSFER failed",
    )?;
    refused(
        side.call(nr::IO_MAPPING_MAP, &[reg(narrow), 0]),
        status::ACCESS_DENIED,
        "a mapping handle without MAP was mapped",
        counter,
    )?;
    counter.mapped += 1;
    side.close_everything();
    Ok(())
}

/// Every processor programmed its PAT with the write-combining entry, a
/// prefetchable aperture maps write-combining -- its page table entry says
/// so, and it reaches the device's own memory -- and an aperture of
/// registers is refused it. On x86-64, with one processor's PAT counted as
/// not programmed, write-combining is refused.
///
/// x86-64 needs the PAT for it; the Arm architectures map normal
/// non-cacheable memory. Runs where a whole-page aperture of each kind
/// exists: every x86-64, AArch64 and ARMv7-A boot, whose virtio-pci
/// functions have a prefetchable 64-bit BAR and registers beside it.
///
/// Verifies: `L.x86_64.127`, L.user.108
fn check_write_combining(counter: &mut Counter) -> Result<(), &'static str> {
    if let Some(programmed) = arch::write_combining_processors()
        && programmed != crate::smp::count()
    {
        return Err("a processor's PAT was not programmed with its write-combining entry");
    }
    let find = |prefetchable: bool| {
        device::devices().iter().find_map(|node| {
            node.apertures()
                .iter()
                .find(|aperture| aperture.whole_pages() && aperture.cacheable() == prefetchable)
                .map(|aperture| (Arc::clone(node), *aperture))
        })
    };
    let side = Side::new()?;
    if let Some((node, aperture)) = find(false) {
        let handle = device_handle(&side, &node)?;
        stage_spec(&side, aperture.phys(), aperture.len())?;
        let mapping = side.handle(
            nr::IO_MAPPING_CREATE,
            &[reg(handle), SPEC],
            "io_mapping_create of a device's registers failed",
        )?;
        refused(
            side.call(nr::IO_MAPPING_MAP_COMBINING, &[reg(mapping), 0]),
            status::INVALID_ARGS,
            "a device's registers were mapped write-combining",
            counter,
        )?;
    }
    if let Some((node, aperture)) = find(true) {
        let handle = device_handle(&side, &node)?;
        stage_spec(&side, aperture.phys(), aperture.len())?;
        let mapping = side.handle(
            nr::IO_MAPPING_CREATE,
            &[reg(handle), SPEC],
            "io_mapping_create of a prefetchable aperture failed",
        )?;
        if arch::write_combining_processors().is_some() {
            object::io_mapping::count_one_unprogrammed_for_check(true);
            let made = side.call(nr::IO_MAPPING_MAP_COMBINING, &[reg(mapping), 0]);
            object::io_mapping::count_one_unprogrammed_for_check(false);
            refused(
                made,
                status::INVALID_ARGS,
                "write-combining was mapped while a processor's PAT was not programmed",
                counter,
            )?;
        }
        let at = side
            .call(nr::IO_MAPPING_MAP_COMBINING, &[reg(mapping), 0])
            .map_err(|_| "io_mapping_map_combining of a prefetchable aperture failed")?
            as u64;
        if !reaches(side.process.space(), at, aperture.phys())? {
            return Err("a write-combining mapping does not reach the device's own memory");
        }
        let flags = mm::flags_in(side.process.space().root_table(), at)
            .ok_or("a write-combining mapping has no page table entry")?;
        // The Arm architectures read write-combining back as what it is
        // there, normal non-cacheable memory.
        if !(flags.write_combining || flags.uncached) || flags.device {
            return Err("a write-combining mapping was not mapped write-combining");
        }
        counter.combined += 1;
        one_memory_type_per_page(&side, mapping, at, aperture.phys(), aperture.len(), counter)?;
    }
    side.close_everything();
    Ok(())
}

/// One memory type per page of device memory: with the aperture at `phys`
/// mapped write-combining at `at` through `mapping`, mapping it uncached or
/// as a cached window, as a render node's `mmap` would, is refused; with it
/// mapped cached or uncached, write-combining is refused; and once nothing
/// maps it, write-combining is allowed again. The window is never touched,
/// so no cached access reaches the device. Mapping the window again, whole
/// or one page of it, adds no claim: the claims are counted per range and
/// type, and a window's range is the whole window.
///
/// Verifies: L.user.109
fn one_memory_type_per_page(
    side: &Side,
    mapping: Handle,
    at: u64,
    phys: u64,
    len: u64,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let space = side.process.space();
    let window_of = |part: u64| {
        let keeper: Arc<dyn Any + Send + Sync> = Arc::new(());
        space.map_window(
            FilePlace::Anywhere(None),
            part,
            WindowPages {
                physical: phys,
                whole: phys,
                whole_len: len,
            },
            VmaFlags::READ_WRITE,
            true,
            keeper,
        )
    };
    let window = || window_of(len);
    let unmap = |at: u64| {
        space
            .unmap(at, len)
            .map_err(|_| "a device mapping could not be unmapped")
    };
    refused(
        side.call(nr::IO_MAPPING_MAP, &[reg(mapping), 0]),
        status::ALREADY_BOUND,
        "an aperture mapped write-combining was mapped uncached beside it",
        counter,
    )?;
    match window() {
        Err(SpaceError::OtherMemoryType) => counter.refusals += 1,
        _ => return Err("an aperture mapped write-combining was mapped cached beside it"),
    }
    unmap(at)?;
    let cached = window().map_err(|_| "a window over an unmapped aperture was refused")?;
    let claims = memory_type::claims();
    let again = window().map_err(|_| "a window was refused beside one of its own type")?;
    let page = window_of(PAGE_SIZE).map_err(|_| "a window's first page was refused")?;
    if memory_type::claims() != claims {
        return Err("mapping a window again, or a page of it, made another claim");
    }
    unmap(again)?;
    space
        .unmap(page, PAGE_SIZE)
        .map_err(|_| "a window's page could not be unmapped")?;
    refused(
        side.call(nr::IO_MAPPING_MAP_COMBINING, &[reg(mapping), 0]),
        status::ALREADY_BOUND,
        "an aperture mapped cached was mapped write-combining beside it",
        counter,
    )?;
    unmap(cached)?;
    let uncached =
        side.call(nr::IO_MAPPING_MAP, &[reg(mapping), 0])
            .map_err(|_| "an aperture nothing maps could not be mapped uncached")? as u64;
    refused(
        side.call(nr::IO_MAPPING_MAP_COMBINING, &[reg(mapping), 0]),
        status::ALREADY_BOUND,
        "an aperture mapped uncached was mapped write-combining beside it",
        counter,
    )?;
    unmap(uncached)?;
    let again = side
        .call(nr::IO_MAPPING_MAP_COMBINING, &[reg(mapping), 0])
        .map_err(|_| "write-combining was refused after every other mapping had gone")?
        as u64;
    unmap(again)
}

/// An interrupt is claimed once, held pending from delivery until the driver
/// acknowledges it, and can be claimed again once its holder lets go.
///
/// Nothing drives a device yet, so nothing makes one interrupt. The delivery
/// here is the kernel's own handler, [`interrupt::on_interrupt`], called as the
/// controller would call it: everything after the hardware — masking, the
/// pending level a wait sees, acknowledgement unmasking — is what is being
/// checked. Only a machine whose devices have vectors runs it: every machine
/// with a PCI function whose MSI-X table can be minted from, where the vector
/// is that function's first entry and masking is the entry's own bit, and
/// ARMv7-A's virtio-mmio transports besides.
///
/// Verifies: L.object.35, L.object.36, L.object.37
fn check_an_interrupt_is_held_until_acknowledged(
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let Some((node, vector)) = device::devices()
        .iter()
        .find_map(|node| node.vector(0).map(|vector| (Arc::clone(node), vector)))
    else {
        return Ok(());
    };
    let side = Side::new()?;
    let handle = device_handle(&side, &node)?;
    let readable = u64::from(Signals::READABLE.0);

    check_interrupt_create_survives_each_failure(&side, handle)?;
    let first = side.handle(
        nr::INTERRUPT_CREATE,
        &[reg(handle), 0],
        "interrupt_create of a device's own vector failed",
    )?;
    refused(
        side.call(nr::INTERRUPT_CREATE, &[reg(handle), 0]),
        status::ALREADY_BOUND,
        "one interrupt line was claimed twice",
        counter,
    )?;
    refused(
        side.call(nr::INTERRUPT_CREATE, &[reg(handle), 99]),
        status::INVALID_ARGS,
        "a vector the device does not have was claimed",
        counter,
    )?;

    stage_deadline(&side, 0)?;
    refused(
        side.call(
            nr::OBJECT_WAIT_ONE,
            &[reg(first), readable, DEADLINE, OBSERVED],
        ),
        status::TIMED_OUT,
        "an interrupt that has not fired said it was pending",
        counter,
    )?;
    // First of the checks that deliver after an acknowledgement, so a
    // delivery lost there fails on its own line.
    check_a_line_is_held_back_where_it_must_be(&side, first, vector.number(), counter)?;

    interrupt::on_interrupt(vector.number());
    stage_deadline(&side, PATIENCE_NANOS)?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[reg(first), readable, DEADLINE, OBSERVED],
        )
        .map_err(|_| "a wait did not see an interrupt that had fired")?;
    let _ = side
        .call(nr::INTERRUPT_ACK, &[reg(first)])
        .map_err(|_| "interrupt_ack failed")?;
    stage_deadline(&side, 0)?;
    refused(
        side.call(
            nr::OBJECT_WAIT_ONE,
            &[reg(first), readable, DEADLINE, OBSERVED],
        ),
        status::TIMED_OUT,
        "an acknowledged interrupt was still pending",
        counter,
    )?;

    check_a_bound_interrupt_reaches_its_port(&side, first, vector.number(), counter)?;
    check_an_interrupt_wakes_its_waiter(&side, first, vector.number(), counter)?;

    // Let go of with a delivery in flight: what a handler holds between finding
    // the line and waking its waiters. The line is free the moment its last
    // handle closes, and the delivery finishes against state nobody can reach
    // any more. When the claim lasted as long as anything held the object, a
    // delivery preempted on another processor made this re-claim fail.
    let in_flight = interrupt::take_delivery(vector.number())
        .ok_or("a claimed line had no delivery to take")?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(first)])
        .map_err(|_| "closing an interrupt failed")?;
    let again = side.handle(
        nr::INTERRUPT_CREATE,
        &[reg(handle), 0],
        "a line its holder had let go of could not be claimed again",
    )?;
    in_flight.fire();
    stage_deadline(&side, 0)?;
    refused(
        side.call(
            nr::OBJECT_WAIT_ONE,
            &[reg(again), readable, DEADLINE, OBSERVED],
        ),
        status::TIMED_OUT,
        "a delivery in flight for a line's old holder reached its new one",
        counter,
    )?;

    // And let go of while another processor drains disposed objects, which a
    // close used to queue its object behind.
    let _ = object::as_if_draining_elsewhere(|| {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(again)])
            .map_err(|_| "closing a reclaimed interrupt failed")?;
        let third = side.handle(
            nr::INTERRUPT_CREATE,
            &[reg(handle), 0],
            "a line let go of while another processor drained could not be claimed again",
        )?;
        side.call(nr::HANDLE_CLOSE, &[reg(third)])
            .map_err(|_| "closing a twice-reclaimed interrupt failed")
    })?;
    counter.interrupts += 1;
    side.close_everything();
    Ok(())
}

/// Wait for a task to stop, up to `deadline`.
fn task_stops(task: &Task, deadline: u64) -> Result<(), &'static str> {
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err("a program that exited kept running");
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// Stage 9's exit criterion: two user processes exchange messages and a
/// handle over a channel.
///
/// Everything else in this file drives the handlers from the kernel; this does
/// not. Both ends are programs running in user mode as tasks of their own,
/// and every native call goes through the real trap path of this
/// architecture: `channel_write` with a handle in the message,
/// `object_wait_one` blocking until the other side has written, `channel_read`,
/// and a VMO read through a handle that arrived from the other process. The
/// kernel only makes the channel and puts one end in each process before
/// either starts, which is what a parent does for a child.
///
/// The sender exits 0 only if the reply it reads is the secret it wrote into
/// its VMO, and the receiver could only have read that secret through the
/// handle it was sent.
///
/// Verifies: L.object.10, L.syscall.1
fn check_two_programs_talk_over_a_channel(counter: &mut Counter) -> Result<(), &'static str> {
    if arch::USER_NATIVE_PROGRAM.is_empty() {
        return Ok(());
    }
    let sender = native_program(b's')?;
    let receiver = native_program(b'r')?;
    let (first, second) = Endpoint::pair().map_err(|_| "could not make a channel")?;
    for (program, end) in [(&sender, first), (&receiver, second)] {
        let placed = program
            .with_handles(|table| table.insert(Object::Channel(end), Rights::CHANNEL))
            .map_err(|_| "no room for a bootstrap handle")?;
        if placed != BOOTSTRAP {
            return Err("a fresh process's first handle is not the one its program was built for");
        }
    }

    // The receiver first, so that its wait usually really blocks. If the
    // sender is scheduled first anyway, the level is already asserted when the
    // receiver looks, and returning at once is also the right answer.
    let receiver_task =
        process::start(&receiver).map_err(|_| "the receiving program could not be started")?;
    let sender_task =
        process::start(&sender).map_err(|_| "the sending program could not be started")?;

    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let received = receiver.wait_for_exit(deadline);
    let sent = sender.wait_for_exit(deadline);
    if received != Some(0) || sent != Some(0) {
        // The program exits with the number of the step that failed, so the
        // statuses are the diagnosis; a static message cannot carry them.
        crate::console::println!(
            "  exit     receiver exited with {received:?}, sender with {sent:?} \
             (steps: sender 1 vmo_create, 2 vmo_write, 3 channel_write, 4 wait, 5 channel_read, \
             6 reply; receiver 11 wait, 12 channel_read, 20 handle count, 13 vmo_read, \
             14 channel_write)"
        );
    }
    if received != Some(0) {
        return Err("the receiving program did not read the message and the handle and reply");
    }
    if sent != Some(0) {
        return Err("the sending program did not get back the secret it put in its VMO");
    }
    task_stops(&receiver_task, deadline)?;
    task_stops(&sender_task, deadline)?;
    // The message carrying the handle, and the reply.
    counter.exchanged += 2;
    Ok(())
}

/// How long a chain of jobs the recursion check builds and frees.
///
/// Recursive dropping overflows a sixteen-kibibyte kernel stack within a few
/// hundred levels. Ten thousand is far past that, and at a couple of hundred
/// bytes a job it is two megabytes of heap.
const JOB_CHAIN: usize = 10_000;

/// A chain of jobs each holding the one above it is freed without a stack
/// frame per job.
///
/// Built with one reference at a time, the way a program looping over
/// `job_create` and `handle_close` would, and dropped from the deepest end. A
/// recursive drop reaches the guard page long before the end of the chain.
///
/// Verifies: L.object.79
fn check_a_long_chain_of_jobs_is_freed_without_recursion() -> Result<(), &'static str> {
    let root = Job::new_root().map_err(|_| "no memory for a job")?;
    let mut deepest = Arc::clone(&root);
    for _ in 0..JOB_CHAIN {
        deepest = deepest
            .new_child()
            .map_err(|_| "a live job refused a child")?;
    }
    drop(root);
    drop(deepest);
    Ok(())
}

/// A new process for the job checks: in the root job, findable, never
/// started, so a kill releases it at once.
fn job_member() -> Result<Arc<Process>, &'static str> {
    process::new_for_check().map_err(|_| "no address space for the job checks")
}

/// Every process is in a job, a fork's child is in its parent's, and a job is
/// populated exactly while one of its members, or of a job beneath it, has
/// not been released: it empties at the last release, not the last reap, and
/// its event queue hears it.
///
/// This is what `cgroup.events` reports and what a service manager waits for
/// (`docs/CGROUPS.md` §2.2), so the count is checked where it changes. The
/// processes are never started, so a kill releases each at once, and every
/// reference to them is still held when the job is read: reaped they are not.
///
/// Verifies: L.object.63, L.object.73, L.object.88
fn check_a_job_counts_its_members() -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "no memory for a job")?;
    let leaf = tree.new_child().map_err(|_| "a live job refused a child")?;
    let parent = job_member()?;
    if !Arc::ptr_eq(&parent.job(), job::root()) {
        return Err("a new process is not in the root job");
    }
    leaf.adopt(&parent)
        .map_err(|_| "a live job refused a process")?;
    if !Arc::ptr_eq(&parent.job(), &leaf) || leaf.live() != 1 {
        return Err("a process moved into a job is not counted there");
    }
    if !leaf.is_populated() || !tree.is_populated() || tree.live() != 0 {
        return Err("a job with a member, or the job above it, is not populated");
    }

    let space = crate::user::space::AddressSpace::new()
        .map_err(|_| "no address space for the job checks")?;
    let child = linux::registry::register(
        Process::forked(&parent, space, false, false).map_err(|_| "no memory for a fork")?,
    );
    if !Arc::ptr_eq(&child.job(), &leaf) || leaf.live() != 2 {
        return Err("a fork's child is not in its parent's job");
    }

    let wakes = leaf.events().wakes();
    let tree_wakes = tree.events().wakes();
    process::kill(&child, KILLED_STATUS);
    if leaf.live() != 1 || !leaf.is_populated() {
        return Err("a released member was not counted out, or was counted out twice");
    }
    if leaf.events().wakes() != wakes {
        return Err("a job that stayed populated woke its event queue");
    }
    process::kill(&parent, KILLED_STATUS);
    if leaf.is_populated() || tree.is_populated() {
        return Err("a job whose members have all been released is still populated");
    }
    if leaf.events().wakes() == wakes || tree.events().wakes() == tree_wakes {
        return Err("a job and the one above it became empty and woke nothing");
    }
    drop((parent, child));
    Ok(())
}

/// `cgroup.kill` ends a job's members and leaves the job usable; `job_kill`
/// ends them and seals it and everything beneath it. A move a sealed job
/// refuses leaves the process where it was.
///
/// Verifies: L.object.66, L.object.77, L.object.86, L.object.99
fn check_the_two_kills() -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "no memory for a job")?;
    let leaf = tree.new_child().map_err(|_| "a live job refused a child")?;
    let member = job_member()?;
    leaf.adopt(&member)
        .map_err(|_| "a live job refused a process")?;
    if leaf.kill_members() != Ok(1) || !member.is_terminated() {
        return Err("killing a job's members did not end its member");
    }
    if leaf.is_populated() || leaf.is_killed() || leaf.is_dying() {
        return Err("killing a job's members left it populated, sealed or dying");
    }
    let next = job_member()?;
    leaf.adopt(&next)
        .map_err(|_| "a job whose members were killed refused a new one")?;

    if tree.kill(KILLED_STATUS) != Ok(1) || !next.is_terminated() {
        return Err("a job kill did not end the member of a job beneath it");
    }
    let late = job_member()?;
    if leaf.adopt(&late).is_ok() {
        return Err("a job beneath a killed one took a new process");
    }
    if !Arc::ptr_eq(&late.job(), job::root()) {
        return Err("a refused move moved the process anyway");
    }
    process::kill(&late, KILLED_STATUS);
    if leaf.is_populated() || tree.is_populated() {
        return Err("a killed job is still populated");
    }

    // A name is held once among a job's children.
    let names = Job::new_root().map_err(|_| "no memory for a job")?;
    let named = names
        .new_named_child("a.slice", job::Budget::Own)
        .map_err(|_| "a live job refused a named child")?;
    if named.name() != Some("a.slice") || names.new_named_child("a.slice", job::Budget::Own).is_ok()
    {
        return Err("a job took a second child with a name it already had");
    }
    drop((member, next, late));
    Ok(())
}

/// A message carrying an endpoint, read into a buffer that faults, is put
/// back whole, and the endpoint read afterwards is the one that was sent.
///
/// The put-back is the path that has to hold the topology lock, since it
/// re-adds an edge the cycle check walks. The race it closes cannot be staged
/// with one thread, but the path itself can: it must still deliver the right
/// object once the buffer is good.
///
/// Verifies: L.object.15
fn check_an_endpoint_survives_a_bad_buffer(
    side: &Side,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let (near, far) = side.channel()?;
    let (carried, carried_far) = side.channel()?;
    side.put_handles(&[carried])?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 0, HANDLES, 1])
        .map_err(|_| "sending an endpoint failed")?;

    refused(
        side.call(nr::CHANNEL_READ, &[reg(far), INBOX, 0, 0x10, 1, ACTUAL]),
        status::FAULT,
        "a read into an unmapped handle buffer was not a fault",
        counter,
    )?;

    let _ = side
        .call(nr::CHANNEL_READ, &[reg(far), INBOX, 0, HANDLES, 1, ACTUAL])
        .map_err(|_| "a message put back after a bad buffer could not be read")?;
    let arrived = Handle(side.get_u32(HANDLES)?);
    side.put(PAYLOAD, b"y")?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(arrived), PAYLOAD, 1, HANDLES, 0])
        .map_err(|_| "the endpoint delivered after a put-back does not write")?;
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[reg(carried_far), INBOX, 8, HANDLES, 0, ACTUAL],
        )
        .map_err(|_| "the endpoint delivered after a put-back is not the one sent")?;

    for end in [near, far, arrived, carried_far] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(end)])
            .map_err(|_| "closing a channel end failed")?;
    }
    Ok(())
}

/// Where the fork check reads a message's bytes: three bytes before the end
/// of the scratch region's first page, so that the bytes run on into the
/// second, which a read faults in only once it has let go of the topology lock.
const STRADDLE: u64 = SCRATCH + PAGE_SIZE - 3;

/// Where the private-file check's file lives.
const PRIVATE_FILE: &[u8] = b"/tmp/ferrix-channel-private";

/// A message read into a private file mapping lands in the mapping's own copy
/// of the page, never in the file.
///
/// Delivery under the topology lock writes only a page it may write in place,
/// and a private file mapping's file page -- present and read-only once read --
/// is not one: the delivery has to put the message back, fault the page in,
/// which copies it into the mapping's shadow, and read again. The bytes
/// straddle the file's two pages, both read back first so both are present.
/// The count of such rounds has to move, the bytes have to arrive, and the
/// file has to keep what it had. It runs before the fork check, so each of the
/// write predicate's two rules has a check of its own to fail.
fn check_a_message_is_read_into_a_private_file_mapping(
    side: &Side,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let ns = crate::fs::namespace();
    let flags = ferrix_vfs::OpenFlags {
        read: true,
        write: true,
        create: true,
        truncate: true,
        ..ferrix_vfs::OpenFlags::default()
    };
    let file = ns
        .open(&ns.context(), None, PRIVATE_FILE, &flags, 0o600)
        .map_err(|_| "could not create a file to map privately")?;
    let outcome = read_into_a_private_file_mapping(side, counter, &file);
    let _ = ns.unlink(&ns.context(), None, PRIVATE_FILE);
    outcome
}

/// The body of [`check_a_message_is_read_into_a_private_file_mapping`], on
/// `file`, with the mapping unmapped whatever happens.
fn read_into_a_private_file_mapping(
    side: &Side,
    counter: &mut Counter,
    file: &Arc<ferrix_vfs::OpenFile>,
) -> Result<(), &'static str> {
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    let kept: Vec<u8> = (0..2 * page).map(|at| (at % 251) as u8).collect();
    if file.write_at(0, &kept) != Ok(kept.len()) {
        return Err("could not fill the file to map privately");
    }
    let object = file
        .inode()
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a tmpfs file has no object to map")?;
    let space = side.process.space();
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            2 * PAGE_SIZE,
            VmaFlags::READ_WRITE,
            object,
            0,
            FileMapping {
                file: Arc::clone(file) as Arc<dyn Any + Send + Sync>,
                may_write: false,
            },
        )
        .map_err(|_| "could not map a file privately")?;
    let outcome = deliver_into_file_pages(side, counter, at, &kept, file);
    let _ = space.unmap(at, 2 * PAGE_SIZE);
    outcome
}

/// Send bytes and read them into the private mapping at `at`, straddling its
/// two file pages, which hold `kept`.
fn deliver_into_file_pages(
    side: &Side,
    counter: &mut Counter,
    at: u64,
    kept: &[u8],
    file: &ferrix_vfs::OpenFile,
) -> Result<(), &'static str> {
    const SENT: &[u8] = b"privately";
    let straddle = at + PAGE_SIZE - 3;
    let offset = usize::try_from(PAGE_SIZE - 3).map_err(|_| "the page size does not fit")?;
    let before = kept
        .get(offset..offset + SENT.len())
        .ok_or("the private file is shorter than the straddle")?;

    let (near, far) = side.channel()?;
    // An endpoint rides along: only a message carrying a handle is delivered
    // under the topology lock, into pages already there to be written, which is
    // the path this check is for. One of bytes alone is copied with faults
    // allowed, and copies the page without ever going round.
    let (carried, carried_far) = side.channel()?;
    side.put_handles(&[carried])?;
    side.put(PAYLOAD, SENT)?;
    let _ = side
        .call(
            nr::CHANNEL_WRITE,
            &[reg(near), PAYLOAD, len(SENT), HANDLES, 1],
        )
        .map_err(|_| "sending an endpoint to read into a private file mapping failed")?;
    // Both file pages read back: each is then present and read-only, the
    // file's own page, which a delivery under the lock must not write through.
    if side.get(straddle, SENT.len())? != before {
        return Err("a private file mapping did not show its file before the read");
    }

    let rounds = native::faulted_rounds();
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[reg(far), straddle, len(SENT), HANDLES, 1, ACTUAL],
        )
        .map_err(|_| "a read into a private file mapping failed")?;
    if native::faulted_rounds() == rounds {
        return Err(
            "a read under the topology lock wrote into a private file mapping's file page in place",
        );
    }
    let mut in_file = vec![0_u8; SENT.len()];
    if file.read_at(offset as u64, &mut in_file) != Ok(in_file.len()) || in_file != before {
        return Err("a read into a private file mapping wrote the file");
    }
    if side.get(straddle, SENT.len())? != SENT {
        return Err("a read into a private file mapping did not deliver the bytes");
    }
    counter.messages += 1;
    let arrived = Handle(side.get_u32(HANDLES)?);
    for end in [near, far, arrived, carried_far] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(end)])
            .map_err(|_| "closing a channel end failed")?;
    }
    Ok(())
}

/// A message carrying an endpoint is delivered into buffers still shared
/// copy-on-write with a fork of the reader's memory, and the fork keeps what
/// it had.
///
/// Delivering such a message holds the topology lock, and a write into a
/// shared page copies it and then waits for every processor to drop the old
/// translation, which no lock may be held across. So a delivery that meets a
/// page it may not write in place puts the message back, lets the lock go,
/// faults the page in, and reads again. The bytes here straddle two pages, and
/// the read faults in only the first before it takes the lock, so the second --
/// read back after the fork, so present but shared -- is met under it: the
/// count of such rounds has to move, the read still has to
/// deliver the bytes and the endpoint that was sent, and the fork's copy of the
/// pages has to be unchanged.
///
/// Verifies: L.object.16
fn check_an_endpoint_is_read_into_pages_shared_by_fork(
    side: &Side,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    const SENT: &[u8] = b"forked";
    const KEPT: &[u8] = b"before";

    let (near, far) = side.channel()?;
    let (carried, carried_far) = side.channel()?;
    side.put_handles(&[carried])?;
    side.put(PAYLOAD, SENT)?;
    let _ = side
        .call(
            nr::CHANNEL_WRITE,
            &[reg(near), PAYLOAD, len(SENT), HANDLES, 1],
        )
        .map_err(|_| "sending an endpoint to read into a forked page failed")?;

    // Written before the fork, so both pages are committed, and so shared
    // copy-on-write once it is made.
    side.put(STRADDLE, KEPT)?;
    let fork = side
        .process
        .space()
        .fork()
        .map_err(|_| "could not fork a space for the check")?;
    // The fork took the parent's translations to the shared pages down, so
    // every page the delivery writes is read back in: each is then present,
    // read-only and still shared, which a delivery under the lock has to
    // notice rather than write through. Left unread, none is present, every
    // delivery faults whatever it checks, and this check would prove nothing.
    if side.get(STRADDLE, KEPT.len())? != KEPT {
        return Err("a fork changed the pages it shares");
    }
    let _ = side.get(HANDLES, 1)?;
    let _ = side.get(ACTUAL, 1)?;

    let rounds = native::faulted_rounds();
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[reg(far), STRADDLE, len(SENT), HANDLES, 1, ACTUAL],
        )
        .map_err(|_| "a read into pages shared by a fork failed")?;
    if native::faulted_rounds() == rounds {
        return Err("a read under the topology lock wrote into a page shared by a fork in place");
    }
    if side.get(STRADDLE, SENT.len())? != SENT {
        return Err("a read into pages shared by a fork did not deliver the bytes");
    }
    let mut kept = [0_u8; KEPT.len()];
    uaccess::copy_from_user(&fork, STRADDLE, &mut kept)
        .map_err(|_| "could not read the fork's copy of the pages")?;
    if kept != KEPT {
        return Err("a read into pages shared by a fork wrote into the fork's copy");
    }

    let arrived = Handle(side.get_u32(HANDLES)?);
    side.put(PAYLOAD, b"y")?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(arrived), PAYLOAD, 1, HANDLES, 0])
        .map_err(|_| "the endpoint read into a forked page does not write")?;
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[reg(carried_far), INBOX, 8, HANDLES, 0, ACTUAL],
        )
        .map_err(|_| "the endpoint read into a forked page is not the one sent")?;
    counter.messages += 1;

    drop(fork);
    for end in [near, far, arrived, carried_far] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(end)])
            .map_err(|_| "closing a channel end failed")?;
    }
    Ok(())
}

/// Stage a user packet at [`PACKET_AT`].
fn stage_packet(side: &Side, key: u64, data: [u64; 2]) -> Result<(), &'static str> {
    let [first, second] = data;
    side.put(PACKET_AT, &key.to_ne_bytes())?;
    side.put(PACKET_AT + 16, &first.to_ne_bytes())?;
    side.put(PACKET_AT + 24, &second.to_ne_bytes())
}

/// The packet at [`PACKET_AT`]: key, kind, signals, and the two data words.
fn read_packet(side: &Side) -> Result<(u64, u32, u32, u64, u64), &'static str> {
    let bytes = side.get(PACKET_AT, 32)?;
    let word = |at: usize| {
        bytes
            .get(at..at + 8)
            .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
            .map(u64::from_ne_bytes)
            .ok_or("a short packet")
    };
    let half = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
            .map(u32::from_ne_bytes)
            .ok_or("a short packet")
    };
    Ok((word(0)?, half(8)?, half(12)?, word(16)?, word(24)?))
}

/// Take a packet from `port` without waiting, into [`PACKET_AT`].
fn take_now(side: &Side, port: Handle) -> Result<usize, Errno> {
    let now = crate::timer::now_nanos();
    side.put(DEADLINE, &now.to_ne_bytes())
        .map_err(|_| Errno::EFAULT)?;
    side.call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET_AT])
}

/// A port gives back what a program queued, and a registration fires once:
/// when its signal comes true, or at once if it already is, for a message
/// arriving and for a peer closing. Refusals: a full port, an asynchronous
/// wait for `WRITABLE`, a port watched through a port, and a registration
/// through a port handle without `WRITE`.
///
/// Verifies: L.object.21, L.object.23, L.object.29, L.object.32, H.OBJ.17
fn check_ports(side: &Side, counter: &mut Counter) -> Result<(), &'static str> {
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    refused(
        take_now(side, port),
        status::TIMED_OUT,
        "an empty port gave a packet",
        counter,
    )?;

    stage_packet(side, 7, [0xA, 0xB])?;
    let _ = side
        .call(nr::PORT_QUEUE, &[reg(port), PACKET_AT])
        .map_err(|_| "port_queue failed")?;
    side.put(PACKET_AT, &[0; 32])?;
    let _ = take_now(side, port).map_err(|_| "a queued packet was not there to take")?;
    if read_packet(side)? != (7, PACKET_USER, 0, 0xA, 0xB) {
        return Err("a user packet came back different from how it was queued");
    }
    counter.packets += 1;

    let (near, far) = side.channel()?;
    let readable = u64::from(Signals::READABLE.0);
    side.put(KEY, &11_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::OBJECT_WAIT_ASYNC, &[reg(far), reg(port), readable, KEY])
        .map_err(|_| "object_wait_async on a channel failed")?;
    refused(
        take_now(side, port),
        status::TIMED_OUT,
        "a registration fired before its signal",
        counter,
    )?;
    side.put(PAYLOAD, b"z")?;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 1, HANDLES, 0])
        .map_err(|_| "a write to a watched channel failed")?;
    let _ = take_now(side, port)
        .map_err(|_| "a message did not fire the registration waiting for it")?;
    let (key, kind, signals, _, _) = read_packet(side)?;
    if key != 11 || kind != PACKET_SIGNAL || !Signals(signals).intersects(Signals::READABLE) {
        return Err("a signal packet did not say what fired it");
    }
    counter.packets += 1;
    let _ = side
        .call(nr::CHANNEL_WRITE, &[reg(near), PAYLOAD, 1, HANDLES, 0])
        .map_err(|_| "a second write to a watched channel failed")?;
    refused(
        take_now(side, port),
        status::TIMED_OUT,
        "a one-shot registration fired twice",
        counter,
    )?;

    // Already readable when registered: the packet is queued at once.
    side.put(KEY, &12_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::OBJECT_WAIT_ASYNC, &[reg(far), reg(port), readable, KEY])
        .map_err(|_| "object_wait_async on a readable channel failed")?;
    let _ =
        take_now(side, port).map_err(|_| "a registration on a state already true did not fire")?;
    if read_packet(side)?.0 != 12 {
        return Err("the packet for an already-true state carried the wrong key");
    }
    counter.packets += 1;

    side.put(KEY, &13_u64.to_ne_bytes())?;
    let peer_closed = u64::from(Signals::PEER_CLOSED.0);
    let _ = side
        .call(
            nr::OBJECT_WAIT_ASYNC,
            &[reg(far), reg(port), peer_closed, KEY],
        )
        .map_err(|_| "object_wait_async for a closing peer failed")?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(near)])
        .map_err(|_| "closing a watched channel's peer failed")?;
    let _ = take_now(side, port)
        .map_err(|_| "a closing peer did not fire the registration waiting for it")?;
    let (key, _, signals, _, _) = read_packet(side)?;
    if key != 13 || !Signals(signals).intersects(Signals::PEER_CLOSED) {
        return Err("a peer-closed packet did not say so");
    }
    counter.packets += 1;

    check_port_refusals(side, port, far, counter)?;
    for end in [port, far] {
        let _ = side
            .call(nr::HANDLE_CLOSE, &[reg(end)])
            .map_err(|_| "closing a port or channel end failed")?;
    }
    Ok(())
}

/// What a port refuses.
///
/// Verifies: L.object.28
fn check_port_refusals(
    side: &Side,
    port: Handle,
    far: Handle,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    refused(
        side.call(
            nr::OBJECT_WAIT_ASYNC,
            &[reg(far), reg(port), u64::from(Signals::WRITABLE.0), KEY],
        ),
        status::INVALID_ARGS,
        "an asynchronous wait for WRITABLE was accepted",
        counter,
    )?;
    refused(
        side.call(
            nr::OBJECT_WAIT_ASYNC,
            &[reg(port), reg(port), u64::from(Signals::READABLE.0), KEY],
        ),
        status::WRONG_TYPE,
        "a port was watched through a port",
        counter,
    )?;
    let reader = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(port), u64::from((Rights::READ | Rights::WAIT).0)],
        "duplicating a port without WRITE failed",
    )?;
    refused(
        side.call(
            nr::OBJECT_WAIT_ASYNC,
            &[reg(far), reg(reader), u64::from(Signals::READABLE.0), KEY],
        ),
        status::ACCESS_DENIED,
        "a registration went through a port handle without WRITE",
        counter,
    )?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(reader)])
        .map_err(|_| "closing a narrowed port failed")?;

    stage_packet(side, 1, [0, 0])?;
    for _ in 0..object::port::PORT_CAPACITY {
        let _ = side
            .call(nr::PORT_QUEUE, &[reg(port), PACKET_AT])
            .map_err(|_| "filling a port failed before it was full")?;
    }
    refused(
        side.call(nr::PORT_QUEUE, &[reg(port), PACKET_AT]),
        status::SHOULD_WAIT,
        "a full port took another user packet",
        counter,
    )
}

/// A `port_wait` with a two-minute deadline is woken by a message a kernel
/// thread writes twenty milliseconds later, through the registration that
/// message fires: the whole chain from a channel write to a woken port.
fn check_a_port_wait_is_woken_by_a_message(counter: &mut Counter) -> Result<(), &'static str> {
    let side = Side::new()?;
    let (near, far) = side.channel()?;
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(KEY, &21_u64.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ASYNC,
            &[reg(far), reg(port), u64::from(Signals::READABLE.0), KEY],
        )
        .map_err(|_| "object_wait_async failed")?;

    WRITTEN_AT.store(0, Ordering::Release);
    *WAKER.lock() = Some((Arc::clone(&side.process), near));
    let _waker = crate::sched::spawn(
        "port waker",
        write_after_a_delay,
        0,
        ferrix_sched::NICE_0_WEIGHT,
    )
    .map_err(|_| "could not start the waker")?;
    stage_deadline(&side, PATIENCE_NANOS)?;
    let woke = side.call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET_AT]);
    let returned = crate::timer::now_nanos();
    *WAKER.lock() = None;
    if woke != Ok(0) {
        return Err("a port wait was not woken by the message its registration watched");
    }
    if !came_back_after_the_write(returned) {
        return Err("a port wait returned before anything had been written");
    }
    if read_packet(&side)?.0 != 21 {
        return Err("a woken port wait took the wrong packet");
    }
    counter.woken += 1;
    counter.packets += 1;
    side.close_everything();
    Ok(())
}

/// The line [`fire_after_a_delay`] delivers on.
static FIRE_LINE: AtomicU32 = AtomicU32::new(0);
/// When it last delivered.
static FIRED_AT: AtomicU64 = AtomicU64::new(0);

/// A kernel thread that delivers an interrupt `extra` nanoseconds after the
/// usual delay, noting when.
///
/// Through the function the interrupt handler calls, from a thread rather than
/// an interrupt, so that the delivery is timed from a known moment. The wakes
/// it issues are the ones a real delivery issues.
fn fire_after_a_delay(extra: usize) {
    let extra = u64::try_from(extra).unwrap_or(0);
    crate::sched::sleep_for(WAKE_AFTER_NANOS.saturating_add(extra));
    FIRED_AT.store(crate::timer::now_nanos(), Ordering::Release);
    interrupt::on_interrupt(FIRE_LINE.load(Ordering::Acquire));
}

/// A delivered interrupt wakes a task waiting on it, and a task waiting on the
/// port it is bound to, rather than leaving either to find it at its wait's
/// recheck.
///
/// [`WAKE_ROUNDS`] deliveries each way, from another thread after a delay. A
/// round counts when its wait was ended by a wake: `wake_all` took the waiting
/// task off the queue, which the queue counts, rather than leaving it listed
/// for the recheck timer to find. At least [`WOKEN_ROUNDS_REQUIRED`] of each
/// kind have to count, and with the wakes taken out of the delivery none do.
///
/// The check used to time the rounds instead, and failed whenever more than
/// two took over two milliseconds from delivery to return. Under a busy host
/// or KVM that is how long a halted processor can take to run again, so it
/// failed on the host rather than on the wake (`FX-0901`). The slowest time is
/// still reported, and nothing judges it.
///
/// # Staggered
///
/// A wait rechecks five milliseconds after it started, and so on, and the
/// firer starts with it. Each round delivers [`WAKE_STAGGER_NANOS`] later than
/// the last, so that the deliveries fall across the recheck period rather than
/// all just before one.
///
/// Called with the interrupt acknowledged, and leaves it acknowledged, bound to
/// a port that is closed.
///
/// Verifies: L.object.38
fn check_an_interrupt_wakes_its_waiter(
    side: &Side,
    interrupt: Handle,
    number: u32,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(KEY, &43_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::INTERRUPT_BIND, &[reg(interrupt), reg(port), KEY])
        .map_err(|_| "binding an interrupt for the wake check failed")?;
    FIRE_LINE.store(number, Ordering::Release);
    let readable = u64::from(Signals::READABLE.0);

    // The port first: every delivery queues a packet on it, and the waits on
    // the interrupt itself leave theirs unread.
    for (through_port, not_woken) in [
        (
            true,
            "an interrupt's port waiter was not woken by the delivery",
        ),
        (false, "an interrupt's waiter was not woken by the delivery"),
    ] {
        let watched = side
            .process
            .with_handles(|table| {
                table
                    .get(if through_port { port } else { interrupt })
                    .map(|(object, _)| object.clone())
            })
            .map_err(|_| "the wake check's handle named nothing")?;
        let mut woken = 0;
        for round in 0..WAKE_ROUNDS {
            let ended_by_a_wake = watched.waiters().waits_ended_by_a_wake();
            let extra = u64::from(round).saturating_mul(WAKE_STAGGER_NANOS);
            let _firer = crate::sched::spawn(
                "interrupt firer",
                fire_after_a_delay,
                usize::try_from(extra).unwrap_or(0),
                ferrix_sched::NICE_0_WEIGHT,
            )
            .map_err(|_| "could not start the interrupt firer")?;
            stage_deadline(side, PATIENCE_NANOS)?;
            let woke = if through_port {
                side.call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET_AT])
            } else {
                side.call(
                    nr::OBJECT_WAIT_ONE,
                    &[reg(interrupt), readable, DEADLINE, OBSERVED],
                )
            };
            let latency =
                crate::timer::now_nanos().saturating_sub(FIRED_AT.load(Ordering::Acquire));
            if woke != Ok(0) {
                return Err("a wait on an interrupt about to be delivered failed");
            }
            if watched.waiters().waits_ended_by_a_wake() != ended_by_a_wake {
                woken += 1;
            }
            counter.delivered += 1;
            counter.slowest_wake = counter.slowest_wake.max(latency);
            let _ = side
                .call(nr::INTERRUPT_ACK, &[reg(interrupt)])
                .map_err(|_| "acknowledging a timed delivery failed")?;
        }
        // Let go of here: a reference to the interrupt is a claim on its line.
        object::dispose([watched]);
        counter.wakes += woken;
        if woken < WOKEN_ROUNDS_REQUIRED {
            return Err(not_woken);
        }
    }
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(port)])
        .map_err(|_| "closing the wake check's port failed")?;
    Ok(())
}

/// A delivery holds a line back exactly where it must, and loses nothing.
///
/// The driver's own sequence first: a delivery, its packet taken, the
/// acknowledgement, then a second delivery before the driver has drained the
/// device -- the one a coalescing line could lose -- and the wait after the
/// drain must find a packet for it. Then, for an edge-triggered MSI-X vector,
/// what is read at its table entry: unmasked after a delivery, still unmasked
/// after [`interrupt::STORM_BOUND`] deliveries nobody acknowledged, masked by
/// the next one with the line's storm count one higher and one packet queued
/// for all of them, and unmasked again by the acknowledgement. A line the
/// controller holds keeps its mask per delivery, which the earlier checks
/// already drive.
///
/// Called with the interrupt acknowledged, and leaves it acknowledged, bound to
/// a port that is closed.
///
/// Verifies: L.object.41
fn check_a_line_is_held_back_where_it_must_be(
    side: &Side,
    interrupt: Handle,
    number: u32,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(KEY, &44_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::INTERRUPT_BIND, &[reg(interrupt), reg(port), KEY])
        .map_err(|_| "binding an interrupt for the masking check failed")?;
    let held = side
        .process
        .with_handles(|table| table.get(interrupt).map(|(object, _)| object.clone()))
        .map_err(|_| "the masking check's handle named nothing")?;
    let result = match &held {
        Object::Interrupt(line) => held_back_rounds(side, (interrupt, line), port, number, counter),
        _ => Err("the masking check's handle was not an interrupt"),
    };
    // Let go of here: a reference to the interrupt is a claim on its line.
    object::dispose([held]);
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(port)])
        .map_err(|_| "closing the masking check's port failed")?;
    result
}

/// [`check_a_line_is_held_back_where_it_must_be`]'s rounds, on the interrupt
/// `handle` names and `line` is, bound to `port`.
fn held_back_rounds(
    side: &Side,
    (handle, line): (Handle, &interrupt::Interrupt),
    port: Handle,
    number: u32,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    // Acknowledged as a driver acknowledges, through its handle.
    let ack = |what: &'static str| {
        side.call(nr::INTERRUPT_ACK, &[reg(handle)])
            .map(|_| ())
            .map_err(|_| what)
    };
    if line.reads_masked() == Some(true) {
        return Err("a claimed, acknowledged line read masked");
    }
    interrupt::on_interrupt(number);
    let _ = take_now(side, port).map_err(|_| "a delivery queued no packet")?;
    if line.coalesces() && line.reads_masked() != Some(false) {
        return Err("one delivery masked an edge-triggered MSI-X vector");
    }
    ack("acknowledging the masking check's first delivery failed")?;
    // The device fires again before the driver drains it: the acknowledgement
    // above is the driver's, and the drain has nothing for the kernel to do.
    interrupt::on_interrupt(number);
    let _ = take_now(side, port)
        .map_err(|_| "an interrupt that fired between acknowledgement and drain was lost")?;
    ack("acknowledging the delivery between acknowledgement and drain failed")?;
    if !line.coalesces() {
        return Ok(());
    }

    let storms = line.storms();
    let started = crate::timer::now_nanos();
    for _ in 0..interrupt::STORM_BOUND {
        interrupt::on_interrupt(number);
    }
    if line.reads_masked() != Some(false) || line.storms() != storms {
        return Err("a line was masked before its storm bound");
    }
    interrupt::on_interrupt(number);
    counter.storm_nanos = crate::timer::now_nanos().saturating_sub(started);
    if line.reads_masked() != Some(true) || line.storms() != storms.wrapping_add(1) {
        crate::console::println!(
            "  irq      {} deliveries without an acknowledgement left the line unmasked (bound {})",
            interrupt::STORM_BOUND + 1,
            interrupt::STORM_BOUND,
        );
        return Err(
            "a line delivered past its storm bound without an acknowledgement was not masked",
        );
    }
    let _ = take_now(side, port).map_err(|_| "a storm queued no packet")?;
    refused(
        take_now(side, port),
        status::TIMED_OUT,
        "a storm queued more than one packet",
        counter,
    )?;
    ack("acknowledging the storm failed")?;
    if line.reads_masked() != Some(false) {
        return Err("the acknowledgement after a storm left the line masked");
    }
    counter.coalesced += 1;
    Ok(())
}

/// A bound interrupt queues one packet on its port per quiet-to-pending
/// transition: a second binding is refused, two deliveries before an
/// acknowledgement queue one packet carrying the key, the interrupt kind and
/// when it fired, and a delivery after the acknowledgement queues another.
///
/// Called with the interrupt freshly acknowledged, so it starts quiet, and
/// leaves it acknowledged.
///
/// Verifies: L.object.39
fn check_a_bound_interrupt_reaches_its_port(
    side: &Side,
    interrupt: Handle,
    number: u32,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(KEY, &41_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::INTERRUPT_BIND, &[reg(interrupt), reg(port), KEY])
        .map_err(|_| "interrupt_bind failed")?;
    refused(
        side.call(nr::INTERRUPT_BIND, &[reg(interrupt), reg(port), KEY]),
        status::ALREADY_BOUND,
        "an interrupt was bound to a second port",
        counter,
    )?;
    formats(side, interrupt, "Interrupt(Interrupt {")?;

    interrupt::on_interrupt(number);
    interrupt::on_interrupt(number);
    stage_deadline(side, PATIENCE_NANOS)?;
    let _ = side
        .call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET_AT])
        .map_err(|_| "a bound interrupt did not reach its port")?;
    let (key, kind, _, fired_at, _) = read_packet(side)?;
    if key != 41 || kind != PACKET_INTERRUPT || fired_at == 0 {
        return Err("an interrupt packet did not carry its key, its kind and when it fired");
    }
    counter.packets += 1;
    refused(
        take_now(side, port),
        status::TIMED_OUT,
        "an interrupt delivered twice before its acknowledgement queued two packets",
        counter,
    )?;

    let _ = side
        .call(nr::INTERRUPT_ACK, &[reg(interrupt)])
        .map_err(|_| "acknowledging a bound interrupt failed")?;
    interrupt::on_interrupt(number);
    stage_deadline(side, PATIENCE_NANOS)?;
    let _ = side
        .call(nr::PORT_WAIT, &[reg(port), DEADLINE, PACKET_AT])
        .map_err(|_| "an acknowledged interrupt did not reach its port again")?;
    counter.packets += 1;
    let _ = side
        .call(nr::INTERRUPT_ACK, &[reg(interrupt)])
        .map_err(|_| "acknowledging the second delivery failed")?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(port)])
        .map_err(|_| "closing an interrupt's port failed")?;
    check_a_pending_interrupt_is_queued_when_bound(side, interrupt, number, counter)
}

/// `interrupt_create` run once per allocation it makes, frames included, the
/// `n`th failing on the `n`th run alone (`user::alloc_check`): each run claims
/// the line or answers `NO_MEMORY`, and one that answered leaves the line
/// free for the next.
fn check_interrupt_create_survives_each_failure(
    side: &Side,
    device: Handle,
) -> Result<(), &'static str> {
    let task = crate::sched::current_id().ok_or("the interrupt check runs outside a task")?;
    for nth in 1..=64 {
        crate::fallible::inject_once(task, nth, true);
        let made = side.call(nr::INTERRUPT_CREATE, &[reg(device), 0]);
        let failed = crate::fallible::stop_injecting() > 0;
        match made {
            Ok(value) => {
                let handle = u64::try_from(value).unwrap_or(u64::MAX);
                let _ = side
                    .call(nr::HANDLE_CLOSE, &[handle])
                    .map_err(|_| "closing an interrupt made with an allocation failing failed")?;
            }
            Err(status::NO_MEMORY) if failed => {}
            Err(_) => {
                return Err(
                    "interrupt_create failed otherwise than for memory, one allocation failing",
                );
            }
        }
        if !failed {
            return Ok(());
        }
    }
    Err("interrupt_create made more allocations than its check allows")
}

/// An interrupt that fired while its port was gone is pending, and binding
/// it to a new port queues its packet there at once.
///
/// Verifies: L.object.40
fn check_a_pending_interrupt_is_queued_when_bound(
    side: &Side,
    interrupt: Handle,
    number: u32,
    counter: &mut Counter,
) -> Result<(), &'static str> {
    interrupt::on_interrupt(number);
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    side.put(KEY, &42_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::INTERRUPT_BIND, &[reg(interrupt), reg(port), KEY])
        .map_err(|_| "binding an interrupt to a second port, the first gone, failed")?;
    let _ = take_now(side, port)
        .map_err(|_| "an interrupt pending when it was bound queued no packet")?;
    let (key, kind, _, _, _) = read_packet(side)?;
    if key != 42 || kind != PACKET_INTERRUPT {
        return Err("a pending interrupt's packet did not carry its new key and kind");
    }
    counter.packets += 1;
    let _ = side
        .call(nr::INTERRUPT_ACK, &[reg(interrupt)])
        .map_err(|_| "acknowledging a pending interrupt failed")?;
    let _ = side
        .call(nr::HANDLE_CLOSE, &[reg(port)])
        .map_err(|_| "closing an interrupt's second port failed")?;
    Ok(())
}
