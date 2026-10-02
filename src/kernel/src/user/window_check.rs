//! Fault windows, checked from user mode: the certification consultant's
//! checks A to L for `docs/NVIDIA.md` §11.4 (K1).
//!
//! A program maps a fault window and reads and writes its pages as the
//! kernel tells it through a control page, as the reverse map check's
//! programs are driven (`user::rmap_check`). The check itself is the window's
//! server: it takes the fault packets off the server's port and answers them
//! through the same window calls the native ABI's three window calls make,
//! so everything from the trap to the answer is the path a driver's clients
//! take. The checks, by the consultant's letters:
//!
//! * **A** a read of a page the window lacks queues one fault, the check puts
//!   the page in and answers, and the read retries and sees the page;
//! * **B** an error answer ends the reader with `SIGBUS`;
//! * **C** a reader the server never answers ends on `SIGKILL`;
//! * **D** the server's death ends a waiting reader with `SIGBUS`, and every
//!   page the window lent leaves the tables of the clients still mapping it;
//! * **E** a revoke under a reader spinning on another processor: the frame
//!   is poisoned the moment the revoke returns, and the reader never sees the
//!   poison;
//! * **F** a kernel copy of a page the window lacks fails, with no fault
//!   queued; one of a page it has copies;
//! * **G** a partial unmap, a fixed mapping over part, a protection change and
//!   a move are each refused, the region and its pages unchanged, nothing
//!   queued;
//! * **H** a whole unmap queues the window's one `UNMAPPED` before the unmap
//!   returns, also with every allocation in it refused;
//! * **I** each insert refusal, and one bad entry in a batch, insert nothing;
//! * **J** a write to a read-only page queues one fault, for a write, and
//!   lands once the check puts the page in writable;
//! * **K** a page of a coherent pool is mapped uncached;
//! * **L** a fork's child is served, a revoke reaches its table, and its exit
//!   queues no `UNMAPPED` while the parent still maps the window.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_native_abi::types::{
    PACKET_WINDOW_FAULT, PACKET_WINDOW_UNMAPPED, PortPacket, WINDOW_FAULT_WRITE,
};
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::mm;
use crate::object::port::Port;
use crate::object::process::Host;
use crate::smp;
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::{image, registry, uaccess};
use crate::user::space::{FilePlace, SpaceError};
use crate::user::vmo::Vmo;
use crate::user::window::{FaultWindow, Server, ServerHandle, WindowError};

/// The control page, as the program expects it.
const CTRL: u64 = 0x7000_0000;
/// The window, as the program expects it.
const WIN: u64 = 0x7100_0000;
/// The window's length in pages.
const PAGES: u64 = 8;
/// Bytes of the control page per role: command, answer, page, word read.
const SLOT: usize = 16;

/// Read the page and report the word.
const READ: u32 = 1;
/// Write the role's marker to the page.
const WRITE: u32 = 2;
/// Read the page over and over until the command changes.
const SPIN: u32 = 3;
/// Exit 0.
const EXIT: u32 = 4;

/// What a program writes: this plus its role.
const MARK: u32 = 0x5EED_0000;
/// What a frame is filled with once a revoke gives it up.
const POISON: u32 = 0xDEAD_BEEF;
/// What page `n` of the pool holds: this plus `n`.
const PATTERN: u32 = 0x0DDC_0000;
/// The status the program exits with when it reads the poison.
const SAW_POISON: i32 = 7;

/// `SIGBUS` and `SIGKILL`, the same on all three architectures.
const SIGBUS: u32 = 7;
/// See [`SIGBUS`].
const SIGKILL: u32 = 9;

/// The roles: the parent, and its fork's child.
const PARENT: usize = 0;
/// See [`PARENT`].
const CHILD: usize = 1;

/// How long a program or the port has to answer.
const ANSWER_NANOS: u64 = 10_000_000_000;
/// How long a wait that must not end is watched.
const QUIET_NANOS: u64 = 50_000_000;
/// How long the whole check may take on its processor.
const PATIENCE_NANOS: u64 = 120_000_000_000;

/// What the check measured, for the boot log.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Report {
    /// Faults the check served.
    pub(crate) served: u32,
    /// Refusals checks G and I saw, each with nothing changed.
    pub(crate) refused: u32,
    /// The processor the check served on, and the reader's for check E.
    pub(crate) processors: (usize, usize),
    /// Frames the measured run did not give back.
    pub(crate) leaked: i64,
}

/// Run it: once to warm the heap up, then measured. `None` on a machine with
/// one processor, or an architecture with no program for it.
pub(crate) fn run() -> Result<Option<Report>, &'static str> {
    let online = smp::topology().map_or(1, smp::Topology::online);
    if arch::USER_WINDOW_PROGRAM.is_empty() || online < 2 {
        return Ok(None);
    }
    let _ = run_pinned()?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    wait_for_window_deaths()?;
    let frames = mm::FrameWindow::open();
    let mut report = run_pinned()?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    wait_for_window_deaths()?;
    report.leaked = frames.kept();
    if report.leaked != 0 {
        frames.report("window");
        return Err("serving fault windows did not give back every frame");
    }
    Ok(Some(report))
}

/// Give the window death task time to finish the deaths a run caused.
fn wait_for_window_deaths() -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
    while crate::user::window::deaths_pending() {
        if crate::timer::now_nanos() >= deadline {
            return Err("the window death task never revoked a dead server's windows");
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// Where the pinned task leaves its answer.
static OUTCOME: SpinLock<Option<Result<Report, &'static str>>> = SpinLock::new(None);

/// Run [`check`] in a task pinned to this processor, and wait for it, for
/// the reason `user::rmap_check` gives.
fn run_pinned() -> Result<Report, &'static str> {
    let here = smp::this_cpu()
        .ok_or("no processor to run the fault window check on")?
        .logical;
    *OUTCOME.lock() = None;
    let task = crate::sched::spawn_on(
        "window-check",
        |here| {
            let outcome = check(here);
            *OUTCOME.lock() = Some(outcome);
        },
        here,
        NICE_0_WEIGHT,
        here,
        CpuSet::of(here),
    )?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let posted = OUTCOME.lock().take();
        if let Some(outcome) = posted {
            while !task.is_dead() {
                if crate::timer::now_nanos() >= deadline {
                    return Err("the fault window check's task never exited after it answered");
                }
                crate::sched::sleep_for(1_000_000);
            }
            return outcome;
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("the fault window check never finished on its processor");
        }
        crate::sched::sleep_for(5_000_000);
    }
}

/// One server, one window, and its pool of pages.
struct Rig {
    /// The server's port.
    port: Arc<Port>,
    /// The server's identity. Dropped, the server dies.
    server: Option<ServerHandle>,
    /// The window.
    window: Arc<FaultWindow>,
    /// The pool the check lends pages from: page `n` holds `PATTERN + n`.
    pool: Arc<Vmo>,
    /// The process the server is, loaded and never started: what a fault
    /// from the server's own process is told apart by.
    _bystander: Arc<Process>,
    /// Faults served.
    served: u32,
}

/// A client: a loaded program with the control page and `rig`'s window
/// mapped where it expects them, not yet started.
struct Client {
    /// The process.
    process: Arc<Process>,
    /// Its control page's object.
    ctrl: Arc<Vmo>,
}

impl Rig {
    /// A server with one window of [`PAGES`] pages, and a pool of sixteen
    /// committed pages.
    fn new() -> Result<Rig, &'static str> {
        let port = Port::new().map_err(|_| "no memory for a fault window server's port")?;
        let bystander = load(1)?;
        let server = Server::new(Arc::clone(&port), bystander.core())
            .map_err(|_| "no memory for a fault window server")?;
        let window = FaultWindow::new(server.server(), PAGES)
            .map_err(|_| "no memory for a fault window")?;
        let pool = Vmo::new_anonymous(16).map_err(|_| "no memory for the check's pool")?;
        for page in 0..16_u32 {
            pool.write_page(u64::from(page), 0, &(PATTERN + page).to_le_bytes())
                .map_err(|_| "the check's pool could not be written")?;
        }
        Ok(Rig {
            port,
            server: Some(server),
            window,
            pool,
            _bystander: bystander,
            served: 0,
        })
    }

    /// A client of this rig's window.
    fn client(&self) -> Result<Client, &'static str> {
        let process = load(1)?;
        let shared = VmaFlags {
            shared: true,
            ..VmaFlags::READ_WRITE
        };
        let id = process
            .space()
            .map_anonymous(CTRL, PAGE_SIZE, shared)
            .map_err(|_| "the control page could not be mapped where the program expects it")?;
        let ctrl = process
            .space()
            .object(id)
            .ok_or("a mapped control page has no object behind it")?;
        ctrl.write_page(0, 0, &[0; 2 * SLOT])
            .map_err(|_| "the control page could not be written")?;
        let _ = process
            .space()
            .map_fault_window(Arc::clone(&self.window), FilePlace::Fixed(WIN), shared)
            .map_err(|_| "a fault window could not be mapped where the program expects it")?;
        Ok(Client { process, ctrl })
    }

    /// The next packet on the port, waiting for it.
    fn packet(&self) -> Result<PortPacket, &'static str> {
        let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
        loop {
            if let Some(packet) = self.port.take() {
                return Ok(packet);
            }
            if crate::timer::now_nanos() >= deadline {
                return Err("a fault window's server was sent no packet");
            }
            crate::sched::sleep_for(1_000_000);
        }
    }

    /// The next fault, for page `page` and the access `write` says.
    fn fault(&self, page: u64, write: bool) -> Result<u64, &'static str> {
        let packet = self.packet()?;
        let wanted = page | if write { WINDOW_FAULT_WRITE } else { 0 };
        if packet.kind != PACKET_WINDOW_FAULT
            || packet.key != self.window.key()
            || packet.data[1] != wanted
        {
            return Err("a fault window's server was sent a packet other than the fault taken");
        }
        Ok(packet.data[0])
    }

    /// Whether the port holds nothing, after a moment to make sure.
    fn quiet(&self) -> bool {
        crate::sched::sleep_for(QUIET_NANOS);
        self.port.is_empty()
    }

    /// Put pool page `index` into the window at `page`.
    fn lend(&self, page: u64, index: u64, write: bool) -> Result<(), &'static str> {
        self.window
            .insert(&[(page, Arc::clone(&self.pool), index, write)])
            .map_err(|_| "a fault window refused a page from the check's pool")
    }

    /// Serve the fault the next packet reports with pool page `index`.
    fn serve(&mut self, page: u64, index: u64, write: bool) -> Result<(), &'static str> {
        let token = self.fault(page, write)?;
        self.lend(page, index, write)?;
        self.window.answer(token, true);
        self.served += 1;
        Ok(())
    }
}

impl Client {
    /// Write word `value` at `offset` in the control page.
    fn put(&self, offset: usize, value: u32) -> Result<(), &'static str> {
        self.ctrl
            .write_page(0, offset, &value.to_le_bytes())
            .map_err(|_| "the control page could not be written")
    }

    /// The word at `offset` in the control page.
    fn get(&self, offset: usize) -> Result<u32, &'static str> {
        let mut word = [0; 4];
        self.ctrl
            .read_page(0, offset, &mut word)
            .map_err(|_| "the control page could not be read")?;
        Ok(u32::from_le_bytes(word))
    }

    /// Tell program `role` to do `command` to window page `page`.
    fn command(&self, role: usize, command: u32, page: u64) -> Result<(), &'static str> {
        self.put(role * SLOT + 4, 0)?;
        self.put(role * SLOT + 8, page as u32)?;
        self.put(role * SLOT, command)
    }

    /// Wait for program `role`'s answer to `command`.
    fn answered(&self, role: usize, command: u32) -> Result<(), &'static str> {
        let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
        loop {
            if self.get(role * SLOT + 4)? == command {
                return Ok(());
            }
            if self.process.is_terminated() && role == PARENT {
                return Err("a fault window's client ended before it answered");
            }
            if crate::timer::now_nanos() >= deadline {
                return Err("a fault window's client stopped answering its commands");
            }
            crate::sched::sleep_for(1_000_000);
        }
    }

    /// The word program `role` last read.
    fn word(&self, role: usize) -> Result<u32, &'static str> {
        self.get(role * SLOT + 12)
    }

    /// Wait for the process to end, and say by which signal, if any.
    fn ended(&self) -> Result<Option<u32>, &'static str> {
        let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
        let _ = self.process.wait_for_exit(deadline);
        if !self.process.is_terminated() {
            return Err("a fault window's client did not end when it should have");
        }
        Ok(self.process.ended_by_signal())
    }

    /// Have the program exit, and end it if it will not.
    fn finish(&self, task: &Arc<crate::sched::Task>) {
        let _ = self.command(PARENT, EXIT, 0);
        let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
        if self.process.wait_for_exit(deadline).is_none() {
            process::kill(&self.process, 128 + SIGKILL as i32);
        }
        while !task.is_dead() {
            crate::sched::sleep_for(1_000_000);
        }
    }
}

/// A process running the window program, with `args` arguments, not started.
fn load(args: usize) -> Result<Arc<Process>, &'static str> {
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_WINDOW_PROGRAM,
    );
    let argv: [&[u8]; 2] = [b"/window", b"child"];
    process::load(
        &file,
        argv.get(..args).unwrap_or(&argv),
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the fault window check's program could not be loaded")
}

/// The whole check, on processor `a`.
fn check(a: usize) -> Result<Report, &'static str> {
    let count = smp::topology().map_or(1, smp::Topology::count);
    let b = (a + 1) % count;
    let mut report = Report {
        served: 0,
        refused: 0,
        processors: (a, b),
        leaked: 0,
    };
    serving(&mut report, b)?;
    error_answer(&mut report)?;
    unanswered()?;
    server_death(&mut report)?;
    Ok(report)
}

/// Checks A, E, F, G, H, I, J, K and L, on one window: a parent on this
/// processor and its fork's child on processor `b`.
fn serving(report: &mut Report, b: usize) -> Result<(), &'static str> {
    let mut rig = Rig::new()?;
    let parent = rig.client()?;
    let child_space = parent
        .process
        .space()
        .fork()
        .map_err(|_| "the address space mapping a fault window could not be forked")?;
    let child_process = registry::register(
        Process::forked(&parent.process, child_space, false, false)
            .map_err(|_| "no memory for a fork")?,
    );
    let stack = child_process
        .startup()
        .ok_or("a forked process has no program start")?
        .stack;
    uaccess::copy_to_user(child_process.space(), stack, &2_u32.to_le_bytes())
        .map_err(|_| "the child's argument count could not be written")?;
    if rig.window.mappings() != 2 {
        return Err("a fork did not count its child as one more mapping of a fault window");
    }
    let child = Client {
        process: child_process,
        ctrl: Arc::clone(&parent.ctrl),
    };
    let parent_task = process::start_on(&parent.process, None)
        .map_err(|_| "a fault window's client could not be started")?;
    let child_task = process::start_on(&child.process, Some(b))
        .map_err(|_| "a fault window's forked client could not be started")?;

    let outcome = serve_both(&mut rig, &parent, &child, report);

    // Whatever happened, the child leaves first (check L's second half), then
    // the parent's window goes (check H).
    let _ = child.command(CHILD, EXIT, 0);
    let deadline = crate::timer::now_nanos().saturating_add(ANSWER_NANOS);
    if child.process.wait_for_exit(deadline).is_none() {
        process::kill(&child.process, 128 + SIGKILL as i32);
    }
    while !child_task.is_dead() {
        crate::sched::sleep_for(1_000_000);
    }
    let unmapped_early = outcome.is_ok() && !rig.quiet();
    let whole = whole_unmap(&rig, &parent);
    parent.finish(&parent_task);
    outcome?;
    if unmapped_early {
        return Err(
            "a fork's child leaving queued UNMAPPED while its parent still mapped the window",
        );
    }
    whole?;
    report.served += rig.served;
    Ok(())
}

/// A, J, K, F, G, I, L and E, in that order, with both programs running.
fn serve_both(
    rig: &mut Rig,
    parent: &Client,
    child: &Client,
    report: &mut Report,
) -> Result<(), &'static str> {
    // A: a read of page 0 is forwarded once, served, and retried.
    parent.command(PARENT, READ, 0)?;
    rig.serve(0, 0, true)?;
    parent.answered(PARENT, READ)?;
    if parent.word(PARENT)? != PATTERN {
        return Err("a read of a fault window page did not see the page its server put in");
    }
    if !rig.quiet() {
        return Err("a fault its server answered was forwarded more than once");
    }

    // J: a write to a read-only page is one fault, for a write.
    rig.lend(1, 1, false)?;
    parent.command(PARENT, READ, 1)?;
    parent.answered(PARENT, READ)?;
    if !rig.quiet() {
        return Err("a read of a page a fault window has read-only was forwarded");
    }
    parent.command(PARENT, WRITE, 1)?;
    rig.serve(1, 1, true)?;
    parent.answered(PARENT, WRITE)?;
    if pool_word(&rig.pool, 1)? != MARK + PARENT as u32 {
        return Err("a write to a page put in writable after its fault did not land");
    }

    coherent_page(rig, parent)?;
    kernel_copies(rig, parent)?;
    report.refused += refusals(rig, parent)?;
    report.refused += insert_refusals(rig)?;

    // L: the child is served from the same window, and a revoke reaches it.
    child.command(CHILD, READ, 0)?;
    child.answered(CHILD, READ)?;
    if child.word(CHILD)? != PATTERN || !rig.quiet() {
        return Err("a fork's child did not see a page its parent's window already had");
    }
    child.command(CHILD, READ, 2)?;
    rig.serve(2, 2, true)?;
    child.answered(CHILD, READ)?;
    if child.word(CHILD)? != PATTERN + 2 {
        return Err("a fork's child was not served from the window it shares");
    }
    rig.window.revoke(2, 1);
    if mm::translate_in(child.process.space().root_table(), WIN + 2 * PAGE_SIZE).is_some() {
        return Err("a revoke left a fault window's page in a fork child's tables");
    }

    revoke_under_reader(rig, child)
}

/// K: a page of a pool a device does not snoop is mapped uncached.
fn coherent_page(rig: &mut Rig, parent: &Client) -> Result<(), &'static str> {
    let pool = Vmo::new_anonymous(1).map_err(|_| "no memory for a coherent pool")?;
    if !pool.make_coherent() {
        return Err("a fresh pool could not be made coherent");
    }
    let _ = pool.commit(0).map_err(|_| "no frame for a coherent pool page")?;
    rig.window
        .insert(&[(3, Arc::clone(&pool), 0, false)])
        .map_err(|_| "a fault window refused a coherent pool's page")?;
    parent.command(PARENT, READ, 3)?;
    parent.answered(PARENT, READ)?;
    match mm::user_flags_in(parent.process.space().root_table(), WIN + 3 * PAGE_SIZE) {
        Some(flags) if flags.uncached && !flags.write && !flags.execute => {}
        _ => return Err("a coherent pool's page was mapped into a client cacheable"),
    }
    rig.window.revoke(3, 1);
    Ok(())
}

/// F: kernel copies never wait for the server.
fn kernel_copies(rig: &Rig, parent: &Client) -> Result<(), &'static str> {
    let space = parent.process.space();
    let mut word = [0_u8; 4];
    let absent = uaccess::copy_from_user(space, WIN + 4 * PAGE_SIZE, &mut word);
    if absent.is_ok() {
        return Err("a kernel copy of a page a fault window lacks did not fail");
    }
    if !rig.quiet() {
        return Err("a kernel copy of a page a fault window lacks was forwarded to its server");
    }
    uaccess::copy_from_user(space, WIN, &mut word)
        .map_err(|_| "a kernel copy of a page a fault window has failed")?;
    if u32::from_le_bytes(word) != PATTERN {
        return Err("a kernel copy of a fault window's page read the wrong page");
    }
    Ok(())
}

/// G: a fault window changes only whole. How many refusals there were.
fn refusals(rig: &Rig, parent: &Client) -> Result<u32, &'static str> {
    let space = parent.process.space();
    let before = space.region_count();
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ
    };
    let refused = [
        space.unmap(WIN + PAGE_SIZE, PAGE_SIZE),
        space.unmap(WIN - PAGE_SIZE, 2 * PAGE_SIZE),
        space.protect(WIN, PAGES * PAGE_SIZE, shared),
        space
            .remap(
                WIN,
                PAGES * PAGE_SIZE,
                2 * PAGES * PAGE_SIZE,
                crate::user::space::Destination::Anywhere,
            )
            .map(|_| ()),
    ];
    if refused.iter().any(|outcome| *outcome != Err(SpaceError::WindowChange))
        || !space.cuts_window(WIN + PAGE_SIZE, PAGE_SIZE)
    {
        return Err("a change to part of a fault window, or to its protection, was not refused");
    }
    if space.region_count() != before
        || mm::translate_in(space.root_table(), WIN).is_none()
        || rig.window.mappings() != 2
        || !rig.quiet()
    {
        return Err("a refused change to a fault window changed its region, pages or mappings");
    }
    Ok(refused.len() as u32 + 1)
}

/// I: every insert refusal inserts nothing. How many refusals there were.
fn insert_refusals(rig: &Rig) -> Result<u32, &'static str> {
    let uncommitted = Vmo::new_anonymous(1).map_err(|_| "no memory for an empty pool")?;
    let file = Vmo::new_anonymous(1).map_err(|_| "no memory for a file's pool")?;
    let _ = file.commit(0).map_err(|_| "no frame for a file's pool page")?;
    file.set_file_len(PAGE_SIZE);
    let good = (5, Arc::clone(&rig.pool), 5, false);
    let batches: [Vec<(u64, Arc<Vmo>, u64, bool)>; 4] = [
        alloc::vec![(5, Arc::clone(&uncommitted), 0, false)],
        alloc::vec![(5, Arc::clone(&file), 0, false)],
        alloc::vec![(PAGES, Arc::clone(&rig.pool), 5, false)],
        alloc::vec![good.clone(), (6, Arc::clone(&uncommitted), 0, false)],
    ];
    for batch in &batches {
        if rig.window.insert(batch) != Err(WindowError::BadEntry) {
            return Err("a fault window took a batch with a bad entry");
        }
        if rig.window.shows(5, false).is_some() || rig.window.shows(6, false).is_some() {
            return Err("a refused insert put a page into a fault window");
        }
    }
    Ok(batches.len() as u32)
}

/// E: a revoke under a reader spinning on another processor. The reader is
/// the child, on processor `b`; the check poisons the frame the moment the
/// revoke returns and lends the page back, so a reader that saw the poison
/// read through a translation the revoke did not take down.
fn revoke_under_reader(rig: &mut Rig, child: &Client) -> Result<(), &'static str> {
    rig.lend(4, 4, false)?;
    child.command(CHILD, SPIN, 4)?;
    crate::sched::sleep_for(10_000_000);
    rig.window.revoke(4, 1);
    rig.pool
        .write_page(4, 0, &POISON.to_le_bytes())
        .map_err(|_| "the revoked page could not be poisoned")?;
    // The reader faults now, unless a stale translation still reads.
    let token = rig.fault(4, false)?;
    rig.pool
        .write_page(4, 0, &(PATTERN + 4).to_le_bytes())
        .map_err(|_| "the revoked page could not be restored")?;
    rig.lend(4, 4, false)?;
    rig.window.answer(token, true);
    rig.served += 1;
    crate::sched::sleep_for(10_000_000);
    child.put(CHILD * SLOT, 0)?;
    child.answered(CHILD, SPIN)?;
    if child.process.is_terminated() {
        return Err(
            "a reader on another processor read the poison after a revoke: its translation \
             outlived the revoke",
        );
    }
    Ok(())
}

/// H: the parent's whole unmap, the window's last mapping, queues its one
/// `UNMAPPED` before it returns -- with every allocation in it refused.
fn whole_unmap(rig: &Rig, parent: &Client) -> Result<(), &'static str> {
    let me = crate::sched::current()
        .ok_or("the fault window check runs in no task")?
        .id;
    crate::fallible::inject(me, 1);
    let unmapped = parent.process.space().unmap(WIN, PAGES * PAGE_SIZE);
    let _ = crate::fallible::stop_injecting();
    unmapped.map_err(|_| "a whole fault window could not be unmapped")?;
    match rig.port.take() {
        Some(packet) if packet.kind == PACKET_WINDOW_UNMAPPED && packet.key == rig.window.key() => {}
        _ => return Err("a fault window's last unmap did not queue UNMAPPED before it returned"),
    }
    if rig.window.mappings() != 0 || !rig.quiet() {
        return Err("a fault window's last unmap queued more than one UNMAPPED");
    }
    rig.window.revoke(0, PAGES);
    Ok(())
}

/// B: an error answer ends the reader with `SIGBUS`.
fn error_answer(report: &mut Report) -> Result<(), &'static str> {
    let rig = Rig::new()?;
    let client = rig.client()?;
    let task = process::start_on(&client.process, None)
        .map_err(|_| "a fault window's client could not be started")?;
    client.command(PARENT, READ, 5)?;
    let token = rig.fault(5, false)?;
    rig.window.answer(token, false);
    let signal = client.ended();
    client.finish(&task);
    if signal? != Some(SIGBUS) {
        return Err("a fault its server refused did not end the reader with SIGBUS");
    }
    report.served += 1;
    Ok(())
}

/// C: a reader its server never answers ends on `SIGKILL`.
fn unanswered() -> Result<(), &'static str> {
    let rig = Rig::new()?;
    let client = rig.client()?;
    let task = process::start_on(&client.process, None)
        .map_err(|_| "a fault window's client could not be started")?;
    client.command(PARENT, READ, 6)?;
    let _ = rig.fault(6, false)?;
    crate::sched::sleep_for(QUIET_NANOS);
    if client.process.is_terminated() {
        return Err("a reader waiting on its fault window's server ended by itself");
    }
    process::kill(&client.process, 128 + SIGKILL as i32);
    let signal = client.ended();
    client.finish(&task);
    if signal? != Some(SIGKILL) {
        return Err("a reader waiting on its fault window's server did not end on SIGKILL");
    }
    Ok(())
}

/// D: the server's death ends a waiting reader with `SIGBUS`, and takes every
/// page the window lent out of the tables of the client still mapping it.
fn server_death(report: &mut Report) -> Result<(), &'static str> {
    let mut rig = Rig::new()?;
    let watcher = rig.client()?;
    let reader = rig.client()?;
    rig.lend(0, 0, false)?;
    let watcher_task = process::start_on(&watcher.process, None)
        .map_err(|_| "a fault window's client could not be started")?;
    let reader_task = process::start_on(&reader.process, None)
        .map_err(|_| "a fault window's client could not be started")?;
    watcher.command(PARENT, READ, 0)?;
    watcher.answered(PARENT, READ)?;
    reader.command(PARENT, READ, 7)?;
    let _ = rig.fault(7, false)?;
    drop(rig.server.take());
    let signal = reader.ended();
    wait_for_window_deaths()?;
    let lent_still = mm::translate_in(watcher.process.space().root_table(), WIN).is_some();
    // A later fault in the dead window is SIGBUS too.
    watcher.command(PARENT, READ, 1)?;
    let watcher_signal = watcher.ended();
    reader.finish(&reader_task);
    watcher.finish(&watcher_task);
    if signal? != Some(SIGBUS) {
        return Err("a fault waiting on a server that died did not end with SIGBUS");
    }
    if lent_still {
        return Err("a dead server's lent page stayed in a client's tables");
    }
    if watcher_signal? != Some(SIGBUS) {
        return Err("a fault in a dead server's window did not end with SIGBUS");
    }
    report.served += rig.served;
    Ok(())
}

/// The first word of pool page `index`.
fn pool_word(pool: &Vmo, index: u64) -> Result<u32, &'static str> {
    let mut word = [0; 4];
    pool.read_page(index, 0, &mut word)
        .map_err(|_| "the check's pool could not be read")?;
    Ok(u32::from_le_bytes(word))
}

/// The ELF class a program for this build is.
fn class_of_this_build() -> Class {
    if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    }
}
