//! Pager-backed VMOs, proved at boot (`docs/microkernel/K3.md` §6; the
//! consultant's OK IF, ledger 2858).
//!
//! A job tree made for the check holds two client jobs, C and D, under one
//! parent. Each pager is served by this task through a process P's native
//! calls (`object::check::Side`, the native dispatch with raw registers);
//! each fault is taken by a thread of a client process of its own
//! (`syscall::check::spawn_in`), which faults its mapping of the pager's VMO
//! as a program's fault does (`oom::user_fault`, a program's own) or copies
//! out of it as a call does (`uaccess`, a kernel copy). So the request, the
//! packet, the wait and its ending are the ones a program meets. The cases:
//!
//! * a: a read fault opens one request and one packet; a supply ends it and
//!   the faulting thread reads what was supplied;
//! * b: two threads of one job share one request; two jobs' faults on one
//!   page open two, and one supply ends both, a second moving nothing;
//! * c: the pager's `FAIL` gives the faulting thread `SIGBUS`, audited;
//! * l: the pager fails one job's request by its id, and the other job's
//!   fault on the same page is still supplied (2858 (1));
//! * d: a pager that never answers: the wait ends never before the pager's
//!   bound and within a generous one, audited, the request gone;
//! * e: the pager's close ends a waiting fault before its bound, a later
//!   fault at once with no packet, and a present page still reads;
//! * f: the faulting process's kill ends its wait;
//! * g: a kernel copy waits and is supplied; into a `NO_COPY_WAIT` VMO it
//!   fails at once with no packet;
//! * h: a trap-dirty VMO's first write asks the pager, `DIRTY` lets it land
//!   and the query shows it; `FAIL` refuses another, which stays Clean;
//! * i: the kernel's own dirty mark, a write-back that takes the write right
//!   away before it returns, a write after it Dirty again, its end cleaning
//!   only the other page, and a decommit taking the Clean page alone;
//! * j: supplies refused, each changing nothing;
//! * k: `vmo_map`'s `MAP_NO_PAGER`;
//! * m: a supply over a present page leaves it, and its aux page, as they
//!   were;
//! * n: the calls' decoding: options, the bound's clamp, no duplicate.
//!
//! K7's ticket (the faulting job pays) is wired in, with its case, when K7
//! lands; until then a packet's second word is the request's own id.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    MAP_NO_PAGER, MAP_READ, MAP_WRITE, PACKET_PAGE_REQUEST, PAGE_REQUEST_DIRTY, PAGE_REQUEST_READ,
    PAGER_OP_DECOMMIT, PAGER_OP_DIRTY, PAGER_OP_FAIL, PAGER_OP_WRITEBACK_BEGIN,
    PAGER_OP_WRITEBACK_END, PAGER_VMO_NO_COPY_WAIT, PAGER_VMO_TRAP_DIRTY, PAGER_WAIT_MAX_NS,
    PAGER_WAIT_MIN_NS, PortPacket, page_request_parts,
};

use super::check::{SCRATCH, Side, reg};
use super::job::{Job, KILLED_STATUS};
use super::port::Port;
use super::process::Host;
use super::{Object, pager};
use crate::audit;
use crate::mm;
use crate::sched::{self, Task};
use crate::sync::SpinLock;
use crate::syscall::check::spawn_in;
use crate::syscall::process;
use crate::syscall::uaccess;
use crate::user::space::Access;
use crate::user::vmo::{PageState, Vmo};

/// Where a client maps the pager's VMO.
const MAP_AT: u64 = 0x6000_0000;
/// The VMO's pages.
const PAGES: u64 = 8;
/// Where a spec is staged in a process's scratch.
const SPEC: u64 = SCRATCH + 0x800;
/// Where a VMO offset is staged.
const OFFSET: u64 = SCRATCH + 0x880;
/// Where `pager_query_dirty` writes its runs.
const RUNS: u64 = SCRATCH + 0x900;
/// Where a page's bytes are staged for `vmo_write`: the second scratch page.
const BYTES: u64 = SCRATCH + PAGE_SIZE;
/// How long the check waits for something it is owed: generous against an
/// emulated processor on a loaded host (2858 (7)).
const PATIENCE_NANOS: u64 = 60_000_000_000;
/// How long a wait that should end promptly may take: generous, and far
/// below the bound it is held against.
const PROMPTLY_NANOS: u64 = 5_000_000_000;
/// The short bound case d gives its pager.
const SHORT_WAIT_NANOS: u64 = 5_000_000;
/// How long the check lets a second fault arrive that must not.
const SETTLE_NANOS: u64 = 50_000_000;
/// How often it looks.
const POLL_NANOS: u64 = 1_000_000;

/// What the boot line counts.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Faults forwarded and supplied.
    pub(crate) supplied: u32,
    /// Waits ended without their page: failed, timed out, closed, killed.
    pub(crate) ended: u32,
    /// `PAGER` records the refusal ring gained.
    pub(crate) audited: u64,
    /// Calls refused as specified.
    pub(crate) refused: u32,
    /// How long case d's wait took, in microseconds, against its 5 ms bound.
    pub(crate) timed_out_micros: u64,
}

/// What a faulting thread is to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    /// Fault the page for reading, as a program's load, then read it.
    Read,
    /// Fault the page for writing, as a program's store, then write it.
    Write,
    /// Copy out of the page, as a call's copy of a user buffer.
    Copy,
}

/// How a faulting thread's fault ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    /// It read this byte.
    Read(u8),
    /// It wrote.
    Wrote,
    /// `SIGBUS`: the fault was refused as unreadable.
    Bus,
    /// The copy failed.
    CopyFailed,
    /// Its process was ending: it left.
    Left,
    /// Anything else: a check failure.
    Other,
}

/// One faulting thread's plan and what came of it, by its process's pid.
#[derive(Clone, Copy, Debug)]
struct Plan {
    /// Whose.
    pid: u32,
    /// What it does.
    act: Act,
    /// Where.
    at: u64,
    /// When it began and ended, on the counter.
    began: u64,
    /// See [`Plan::began`].
    ended: u64,
    /// How it ended, once it has.
    outcome: Option<Outcome>,
}

/// The plans of the threads now running.
static PLANS: SpinLock<[Option<Plan>; 4]> = SpinLock::new([None; 4]);

/// The byte a supply of page `page` fills it with.
const fn byte_of(page: u64) -> u8 {
    0xA0_u8.wrapping_add(page as u8)
}

/// The tree, the parent, the two client jobs.
struct World {
    /// The tree the jobs hang from.
    tree: Arc<Job>,
    /// C and D's parent.
    parent: Arc<Job>,
    /// Client job C.
    c: Arc<Job>,
    /// Client job D.
    d: Arc<Job>,
}

/// One pager and its VMO, served by this task through process P.
struct Served {
    /// The pager's process.
    side: Side,
    /// Its port, as a handle and as the object.
    port: (Handle, Arc<Port>),
    /// The pager's handle; `None` once closed.
    pager: Option<Handle>,
    /// What the pager's VMOs share, for the counts.
    core: Arc<pager::PagerCore>,
    /// The VMO, as a handle and as the object.
    vmo: (Handle, Arc<Vmo>),
}

/// Run every case.
///
/// # Errors
///
/// Which property failed.
///
/// Verifies: H.MEM.22, H.MEM.23
pub(crate) fn run() -> Result<Report, &'static str> {
    let tree = Job::new_root().map_err(|_| "no memory for a job")?;
    let parent = tree.new_child().map_err(|_| "a job refused a child")?;
    let world = World {
        c: parent.new_child().map_err(|_| "a job refused a child")?,
        d: parent.new_child().map_err(|_| "a job refused a child")?,
        parent,
        tree,
    };
    let mut report = Report::default();
    let before = pager_records();
    case_a_supplied(&world, &mut report)?;
    case_b_shared_within_a_job(&world, &mut report)?;
    case_c_failed(&world, &mut report)?;
    case_l_one_job_failed(&world, &mut report)?;
    case_d_timed_out(&world, &mut report)?;
    case_e_closed(&world, &mut report)?;
    case_f_killed(&world, &mut report)?;
    case_g_copies(&world, &mut report)?;
    case_h_trap_dirty(&world)?;
    case_i_write_back()?;
    report.refused += case_j_supply_refused()?;
    report.refused += case_k_no_pager()?;
    case_m_present_skipped()?;
    report.refused += case_n_decoding()?;
    report.audited = pager_records().saturating_sub(before);
    // c, l, d, e's waiter and h's refusal: one record each.
    if report.audited < 5 {
        return Err("a pager's failed, timed-out or closed request was not audited");
    }
    let _ = (&world.tree, &world.parent);
    Ok(report)
}

impl Served {
    /// A pager over a new port, bounded by `wait_ns`, and a VMO of [`PAGES`]
    /// pages over it with `options`.
    fn new(wait_ns: u64, options: u64) -> Result<Served, &'static str> {
        let side = Side::new()?;
        side.put(SCRATCH, &[0; 0x980])?;
        let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
        let port_object = side
            .process
            .with_handles(|table| match table.get(port) {
                Ok((Object::Port(port), _)) => Some(Arc::clone(port)),
                _ => None,
            })
            .ok_or("a port handle named no port")?;
        side.put(SPEC, &wait_ns.to_ne_bytes())?;
        let pager = side.handle(
            nr::PAGER_CREATE,
            &[reg(port), SPEC, 0],
            "pager_create failed",
        )?;
        let core = side
            .process
            .with_handles(|table| match table.get(pager) {
                Ok((Object::Pager(pager), _)) => Some(Arc::clone(pager.core())),
                _ => None,
            })
            .ok_or("a pager handle named no pager")?;
        put_words(&side, SPEC, &[0x6b33, PAGES, options])?;
        let vmo = side.handle(
            nr::PAGER_CREATE_VMO,
            &[reg(pager), SPEC],
            "pager_create_vmo failed",
        )?;
        let vmo_object = vmo_of(&side, vmo)?;
        Ok(Served {
            side,
            port: (port, port_object),
            pager: Some(pager),
            core,
            vmo: (vmo, vmo_object),
        })
    }

    /// The pager's handle.
    fn pager(&self) -> Result<Handle, &'static str> {
        self.pager.ok_or("the pager was closed")
    }

    /// The next page request on its port, waiting for it.
    fn packet(&self) -> Result<PortPacket, &'static str> {
        let deadline = sched_now().saturating_add(PATIENCE_NANOS);
        loop {
            if let Some(packet) = self.port.1.take() {
                if packet.kind != PACKET_PAGE_REQUEST || packet.key != 0x6b33 {
                    return Err("a pager's port held a packet that is no page request of its VMO");
                }
                return Ok(packet);
            }
            if sched_now() >= deadline {
                return Err("a fault on an absent page sent its pager no packet");
            }
            sched::sleep_for(POLL_NANOS);
        }
    }

    /// Whether no packet arrives for a while.
    fn quiet(&self) -> bool {
        sched::sleep_for(SETTLE_NANOS);
        self.port.1.is_empty()
    }

    /// Supply `count` pages from `page`, filled with [`byte_of`] each.
    fn supply(&self, page: u64, count: u64) -> Result<Arc<Vmo>, &'static str> {
        let aux = self.side.handle(
            nr::VMO_CREATE,
            &[count * PAGE_SIZE],
            "vmo_create for an aux VMO failed",
        )?;
        for at in 0..count {
            self.side
                .put(BYTES, &[byte_of(page + at); PAGE_SIZE as usize])?;
            self.side.put(OFFSET, &(at * PAGE_SIZE).to_ne_bytes())?;
            let _ = self
                .side
                .call(nr::VMO_WRITE, &[reg(aux), BYTES, PAGE_SIZE, OFFSET])
                .map_err(|_| "vmo_write into an aux VMO failed")?;
        }
        let aux_object = vmo_of(&self.side, aux)?;
        put_words(
            &self.side,
            SPEC,
            &[page * PAGE_SIZE, count * PAGE_SIZE, 0, 0],
        )?;
        let supplied = self.side.call(
            nr::PAGER_SUPPLY,
            &[reg(self.pager()?), reg(self.vmo.0), reg(aux), SPEC],
        );
        close(&self.side, aux);
        let _ = supplied.map_err(|_| "pager_supply refused a good supply")?;
        Ok(aux_object)
    }

    /// `pager_op(op)` over `count` pages from `page`.
    fn op(&self, op: u64, page: u64, count: u64, value: u64, request: u64) -> Result<usize, Errno> {
        put_words(
            &self.side,
            SPEC,
            &[page * PAGE_SIZE, count * PAGE_SIZE, value, request],
        )
        .map_err(|_| Errno::EFAULT)?;
        let pager = self.pager().map_err(|_| Errno::EBADF)?;
        self.side
            .call(nr::PAGER_OP, &[reg(pager), reg(self.vmo.0), op, SPEC])
    }

    /// The Dirty and awaiting-clean runs as `(first page, pages, state)`.
    fn dirty(&self) -> Result<Vec<(u64, u64, u64)>, &'static str> {
        put_words(&self.side, SPEC, &[0, PAGES * PAGE_SIZE])?;
        let count = self
            .side
            .call(
                nr::PAGER_QUERY_DIRTY,
                &[reg(self.pager()?), reg(self.vmo.0), SPEC, RUNS, 8],
            )
            .map_err(|_| "pager_query_dirty failed")?;
        let mut runs = Vec::new();
        for at in 0..count as u64 {
            let words = get_words::<3>(&self.side, RUNS + 24 * at)?;
            runs.push((words[0] / PAGE_SIZE, words[1] / PAGE_SIZE, words[2]));
        }
        Ok(runs)
    }

    /// A client process in `job` mapping the VMO read-write at [`MAP_AT`].
    fn client(&self, job: &Arc<Job>) -> Result<Side, &'static str> {
        let side = Side::new()?;
        side.process
            .move_new_to(job)
            .map_err(|_| "a job refused a process")?;
        side.put(SCRATCH, &[0; 0x100])?;
        let vmo = side
            .process
            .with_handles(|table| table.insert(Object::Vmo(Arc::clone(&self.vmo.1)), Rights::VMO))
            .map_err(|_| "no room for a VMO in a client")?;
        side.put(OFFSET, &0_u64.to_ne_bytes())?;
        let _ = side
            .call(
                nr::VMO_MAP,
                &[
                    reg(vmo),
                    MAP_AT,
                    PAGES * PAGE_SIZE,
                    u64::from(MAP_READ | MAP_WRITE),
                    OFFSET,
                ],
            )
            .map_err(|_| "vmo_map of a pager's VMO failed")?;
        Ok(side)
    }

    /// Close the pager's handle: its death.
    fn close_pager(&mut self) {
        if let Some(pager) = self.pager.take() {
            close(&self.side, pager);
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.side.close_everything();
    }
}

/// The counter's reading.
fn sched_now() -> u64 {
    crate::timer::now_nanos()
}

/// The VMO `handle` names in `side`.
fn vmo_of(side: &Side, handle: Handle) -> Result<Arc<Vmo>, &'static str> {
    side.process
        .with_handles(|table| match table.get(handle) {
            Ok((Object::Vmo(vmo), _)) => Some(Arc::clone(vmo)),
            _ => None,
        })
        .ok_or("a handle named no VMO")
}

/// Stage native-endian words at `at`.
fn put_words(side: &Side, at: u64, words: &[u64]) -> Result<(), &'static str> {
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
    side.put(at, &bytes)
}

/// Read `N` native-endian words at `at`.
fn get_words<const N: usize>(side: &Side, at: u64) -> Result<[u64; N], &'static str> {
    let bytes = side.get(at, 8 * N)?;
    let mut words = [0_u64; N];
    for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
        let mut eight = [0_u8; 8];
        eight.copy_from_slice(chunk);
        *word = u64::from_ne_bytes(eight);
    }
    Ok(words)
}

/// Close `handle` in `side`.
fn close(side: &Side, handle: Handle) {
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
}

/// The `PAGER` records the refusal ring holds.
fn pager_records() -> u64 {
    let mut records = [audit::Record::EMPTY; 16];
    let mut from = 0;
    let mut count = 0;
    loop {
        let read = audit::read(audit::Which::Refusals, from, &mut records);
        if read.copied == 0 {
            return count;
        }
        count += records
            .iter()
            .take(read.copied)
            .filter(|record| record.is(audit::PAGER))
            .count() as u64;
        from = read.next;
    }
}

/// Start a thread of `client` that does `act` at page `page` of its mapping.
fn fault_in(client: &Side, act: Act, page: u64) -> Result<Arc<Task>, &'static str> {
    let pid = Host::core(&*client.process).pid();
    {
        let mut plans = PLANS.lock();
        let slot = plans
            .iter_mut()
            .find(|plan| plan.is_none())
            .ok_or("more faulting threads than the check keeps plans for")?;
        *slot = Some(Plan {
            pid,
            act,
            at: MAP_AT + page * PAGE_SIZE,
            began: 0,
            ended: 0,
            outcome: None,
        });
    }
    spawn_in(&client.process, "pager faulter", faulter, None)
}

/// How `client`'s thread ended, and how long it took, waiting for it.
fn outcome(client: &Side, task: &Arc<Task>) -> Result<(Outcome, u64), &'static str> {
    let pid = Host::core(&*client.process).pid();
    let deadline = sched_now().saturating_add(PATIENCE_NANOS);
    loop {
        let done = {
            let mut plans = PLANS.lock();
            let slot = plans
                .iter_mut()
                .find(|plan| plan.is_some_and(|plan| plan.pid == pid && plan.outcome.is_some()));
            slot.and_then(Option::take)
                .and_then(|plan| Some((plan.outcome?, plan.ended.saturating_sub(plan.began))))
        };
        if let Some(done) = done {
            while !task.is_dead() && sched_now() < deadline {
                sched::sleep_for(POLL_NANOS);
            }
            return Ok(done);
        }
        if sched_now() >= deadline {
            return Err("a faulting thread never finished");
        }
        sched::sleep_for(POLL_NANOS);
    }
}

/// Whether `client`'s thread is still waiting.
fn still_waiting(client: &Side) -> bool {
    let pid = Host::core(&*client.process).pid();
    PLANS
        .lock()
        .iter()
        .flatten()
        .any(|plan| plan.pid == pid && plan.outcome.is_none())
}

/// The faulting thread: its plan, then the end of its process.
fn faulter(_argument: usize) {
    let pid = sched::current()
        .and_then(|task| task.core_process().map(super::process::Process::pid))
        .unwrap_or(0);
    let plan = PLANS
        .lock()
        .iter()
        .flatten()
        .find(|plan| plan.pid == pid)
        .copied();
    let began = sched_now();
    let outcome = match (
        plan,
        sched::current().and_then(|task| task.address_space().cloned()),
    ) {
        (Some(plan), Some(space)) => act(plan.act, plan.at, &space),
        _ => Outcome::Other,
    };
    let ended = sched_now();
    if let Some(slot) = PLANS
        .lock()
        .iter_mut()
        .flatten()
        .find(|plan| plan.pid == pid)
    {
        slot.began = began;
        slot.ended = ended;
        slot.outcome = Some(outcome);
    }
    process::exit_current(0)
}

/// Do `act` at `at` in `space`.
fn act(act: Act, at: u64, space: &crate::user::space::AddressSpace) -> Outcome {
    let mut byte = [0_u8; 1];
    match act {
        Act::Copy => match uaccess::copy_from_user(space, at, &mut byte) {
            Ok(()) => Outcome::Read(byte[0]),
            Err(_) => Outcome::CopyFailed,
        },
        Act::Read | Act::Write => {
            let access = if act == Act::Write {
                Access::WRITE
            } else {
                Access::READ
            };
            match crate::object::oom::user_fault(space, at, access) {
                Ok(()) if sched::work::own_end() => Outcome::Left,
                Ok(()) if act == Act::Write => match uaccess::copy_to_user(space, at, &[0x5A]) {
                    Ok(()) => Outcome::Wrote,
                    Err(_) => Outcome::Other,
                },
                Ok(()) => match uaccess::copy_from_user(space, at, &mut byte) {
                    Ok(()) => Outcome::Read(byte[0]),
                    Err(_) => Outcome::Other,
                },
                Err(crate::user::space::SpaceError::Unreadable(_)) => Outcome::Bus,
                Err(_) => Outcome::Other,
            }
        }
    }
}

/// Wait until `served`'s pager has `waiting` threads waiting on it.
fn until_waiting(served: &Served, waiting: usize) -> Result<(), &'static str> {
    let deadline = sched_now().saturating_add(PATIENCE_NANOS);
    while served.core.waiting() < waiting {
        if sched_now() >= deadline {
            return Err("a fault never waited on its pager");
        }
        sched::sleep_for(POLL_NANOS);
    }
    Ok(())
}

/// A read packet's `(first page, pages, request id)`, or the failure.
fn read_packet(packet: &PortPacket) -> Result<(u64, u32, u64), &'static str> {
    let (command, pages) = page_request_parts(packet.signals);
    if command != PAGE_REQUEST_READ {
        return Err("a read fault's packet did not ask to read");
    }
    Ok((packet.data[0] / PAGE_SIZE, pages, packet.data[1]))
}

/// Case a.
///
/// Verifies: L.user.146, L.user.147, L.user.148, L.user.153, L.object.230
fn case_a_supplied(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let client = served.client(&world.c)?;
    let task = fault_in(&client, Act::Read, 2)?;
    let (first, pages, id) = read_packet(&served.packet()?)?;
    if first != 2 || pages as u64 != PAGES - 2 || id == 0 {
        return Err(
            "a: the packet named the wrong run: the faulting page and the absent ones after it",
        );
    }
    let _ = served.supply(2, 1)?;
    if outcome(&client, &task)?.0 != Outcome::Read(byte_of(2)) {
        return Err("a: a supplied fault did not read what the pager supplied");
    }
    if !served.quiet() || served.core.outstanding() != 0 || served.port.1.lost() != 0 {
        return Err("a: a supplied request stayed outstanding, sent a second packet, or lost one");
    }
    report.supplied += 1;
    Ok(())
}

/// Case b.
///
/// Verifies: L.user.146
fn case_b_shared_within_a_job(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let (one, two) = (served.client(&world.c)?, served.client(&world.c)?);
    let first = fault_in(&one, Act::Read, 0)?;
    let _ = served.packet()?;
    let second = fault_in(&two, Act::Read, 0)?;
    until_waiting(&served, 2)?;
    if !served.quiet() {
        return Err("b: two faults of one job on one page sent two packets");
    }
    let _ = served.supply(0, 1)?;
    for (side, task) in [(&one, &first), (&two, &second)] {
        if outcome(side, task)?.0 != Outcome::Read(byte_of(0)) {
            return Err("b: a fault sharing a request was not resumed by its supply");
        }
    }
    // Two jobs on one page: two requests, two packets; one supply ends both,
    // and a second supply of the page moves nothing.
    let (c, d) = (served.client(&world.c)?, served.client(&world.d)?);
    let in_c = fault_in(&c, Act::Read, 4)?;
    let _ = served.packet()?;
    let in_d = fault_in(&d, Act::Read, 4)?;
    let _ = served.packet().map_err(
        |_| "b: a second job's fault on a page another job asked for joined its request",
    )?;
    if served.core.outstanding() != 2 {
        return Err("b: two jobs' faults on one page did not open two requests");
    }
    let _ = served.supply(4, 1)?;
    for (side, task) in [(&c, &in_c), (&d, &in_d)] {
        if outcome(side, task)?.0 != Outcome::Read(byte_of(4)) {
            return Err("b: a job's fault was not resumed by the supply of its page");
        }
    }
    let aux = served.supply(4, 1)?;
    if aux.page(0).is_none() {
        return Err("b: a supply of a present page moved its aux page");
    }
    report.supplied += 4;
    Ok(())
}

/// Case c.
///
/// Verifies: L.user.149
fn case_c_failed(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let client = served.client(&world.c)?;
    let task = fault_in(&client, Act::Read, 1)?;
    let _ = served.packet()?;
    let _ = served
        .op(PAGER_OP_FAIL, 1, 1, u64::from(Errno::EIO.0), 0)
        .map_err(|_| "c: pager_op FAIL was refused")?;
    if outcome(&client, &task)?.0 != Outcome::Bus {
        return Err("c: a fault its pager failed was not SIGBUS");
    }
    report.ended += 1;
    Ok(())
}

/// Case l.
///
/// Verifies: L.user.149
fn case_l_one_job_failed(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let (c, d) = (served.client(&world.c)?, served.client(&world.d)?);
    let in_c = fault_in(&c, Act::Read, 3)?;
    let (_, _, id_c) = read_packet(&served.packet()?)?;
    let in_d = fault_in(&d, Act::Read, 3)?;
    let _ = served.packet()?;
    let _ = served
        .op(PAGER_OP_FAIL, 3, 1, u64::from(Errno::EIO.0), id_c)
        .map_err(|_| "l: pager_op FAIL of one request was refused")?;
    if outcome(&c, &in_c)?.0 != Outcome::Bus {
        return Err("l: the failed job's fault was not SIGBUS");
    }
    if !still_waiting(&d) {
        return Err("l: one job's failed request failed another job's fault on the same page");
    }
    let _ = served.supply(3, 1)?;
    if outcome(&d, &in_d)?.0 != Outcome::Read(byte_of(3)) {
        return Err("l: the other job's fault was not supplied after the first failed");
    }
    report.ended += 1;
    report.supplied += 1;
    Ok(())
}

/// Case d.
///
/// Verifies: L.user.150
fn case_d_timed_out(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let served = Served::new(SHORT_WAIT_NANOS, 0)?;
    if served.core.wait_ns() != SHORT_WAIT_NANOS {
        return Err("d: a pager's bound was not kept as given");
    }
    let client = served.client(&world.c)?;
    let task = fault_in(&client, Act::Read, 5)?;
    let (ended, took) = outcome(&client, &task)?;
    if ended != Outcome::Bus {
        return Err("d: a fault its pager never answered did not end in SIGBUS");
    }
    if took < SHORT_WAIT_NANOS {
        return Err("d: a fault's wait ended before its pager's bound");
    }
    if took > PROMPTLY_NANOS {
        return Err("d: a fault's wait outlasted its pager's bound by seconds");
    }
    if served.core.outstanding() != 0 {
        return Err("d: a timed-out request stayed outstanding");
    }
    report.timed_out_micros = took / 1_000;
    report.ended += 1;
    Ok(())
}

/// Case e.
///
/// Verifies: L.user.156
fn case_e_closed(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let mut served = Served::new(0, 0)?;
    let _ = served.supply(6, 1)?;
    let client = served.client(&world.c)?;
    let task = fault_in(&client, Act::Read, 0)?;
    let _ = served.packet()?;
    let closed_at = sched_now();
    served.close_pager();
    let (ended, _) = outcome(&client, &task)?;
    if ended != Outcome::Bus || sched_now().saturating_sub(closed_at) > PROMPTLY_NANOS {
        return Err("e: a fault waiting on a pager that closed did not end in SIGBUS at once");
    }
    let later = served.client(&world.c)?;
    let task = fault_in(&later, Act::Read, 1)?;
    if outcome(&later, &task)?.0 != Outcome::Bus || !served.quiet() {
        return Err("e: a fault after its pager closed waited, or sent a packet");
    }
    let present = served.client(&world.c)?;
    let task = fault_in(&present, Act::Read, 6)?;
    if outcome(&present, &task)?.0 != Outcome::Read(byte_of(6)) {
        return Err("e: a present page of a closed pager's VMO did not read");
    }
    report.ended += 2;
    Ok(())
}

/// Case f.
///
/// Verifies: L.user.151
fn case_f_killed(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let client = served.client(&world.c)?;
    let task = fault_in(&client, Act::Read, 7)?;
    let _ = served.packet()?;
    until_waiting(&served, 1)?;
    process::kill(&client.process, KILLED_STATUS);
    let deadline = sched_now().saturating_add(PROMPTLY_NANOS);
    while !task.is_dead() {
        if sched_now() >= deadline {
            return Err("f: a fault waiting on its pager outlived its process's kill");
        }
        sched::sleep_for(POLL_NANOS);
    }
    let _ = PLANS
        .lock()
        .iter_mut()
        .find(|plan| plan.is_some_and(|plan| plan.pid == Host::core(&*client.process).pid()))
        .map(Option::take);
    if served.core.outstanding() != 0 {
        return Err("f: a request whose only waiter was killed stayed outstanding");
    }
    report.ended += 1;
    Ok(())
}

/// Case g.
///
/// Verifies: L.user.152
fn case_g_copies(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let client = served.client(&world.c)?;
    let task = fault_in(&client, Act::Copy, 3)?;
    let _ = served.packet()?;
    let _ = served.supply(3, 1)?;
    if outcome(&client, &task)?.0 != Outcome::Read(byte_of(3)) {
        return Err("g: a kernel copy from an absent pager page did not wait for its supply");
    }
    let never = Served::new(0, PAGER_VMO_NO_COPY_WAIT)?;
    let client = never.client(&world.c)?;
    let task = fault_in(&client, Act::Copy, 0)?;
    if outcome(&client, &task)?.0 != Outcome::CopyFailed || !never.quiet() {
        return Err("g: a kernel copy from a NO_COPY_WAIT VMO's absent page waited, or asked");
    }
    report.supplied += 1;
    report.ended += 1;
    Ok(())
}

/// Case h.
///
/// Verifies: L.user.154
fn case_h_trap_dirty(world: &World) -> Result<(), &'static str> {
    let served = Served::new(0, PAGER_VMO_TRAP_DIRTY)?;
    let _ = served.supply(0, 2)?;
    let client = served.client(&world.c)?;
    let task = fault_in(&client, Act::Write, 0)?;
    let packet = served.packet()?;
    if page_request_parts(packet.signals) != (PAGE_REQUEST_DIRTY, 1) || packet.data[0] != 0 {
        return Err("h: the first write to a Clean trap-dirty page did not ask the pager");
    }
    let _ = served
        .op(PAGER_OP_DIRTY, 0, 1, 0, 0)
        .map_err(|_| "h: pager_op DIRTY was refused")?;
    if outcome(&client, &task)?.0 != Outcome::Wrote {
        return Err("h: a write the pager let be Dirty did not land");
    }
    if served.dirty()? != [(0, 1, ferrix_native_abi::types::DIRTY_STATE_DIRTY)] {
        return Err("h: the query did not show the written page alone as Dirty");
    }
    let refused = served.client(&world.c)?;
    let task = fault_in(&refused, Act::Write, 1)?;
    let (_, _, id) = {
        let packet = served.packet()?;
        (0, 0, packet.data[1])
    };
    let _ = served
        .op(PAGER_OP_FAIL, 1, 1, u64::from(Errno::ENOSPC.0), id)
        .map_err(|_| "h: pager_op FAIL of a dirty request was refused")?;
    if outcome(&refused, &task)?.0 != Outcome::Bus || served.vmo.1.page_state(1) != PageState::Clean
    {
        return Err("h: a write the pager refused landed, or left its page other than Clean");
    }
    Ok(())
}

/// Case i, through the pager's own mapping of its VMO, faulted by this
/// task as a kernel copy is.
///
/// Verifies: L.user.154, L.user.155
fn case_i_write_back() -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let _ = served.supply(0, 2)?;
    let side = &served.side;
    side.put(OFFSET, &0_u64.to_ne_bytes())?;
    let protection = u64::from(MAP_READ | MAP_WRITE);
    let _ = side
        .call(
            nr::VMO_MAP,
            &[reg(served.vmo.0), MAP_AT, 2 * PAGE_SIZE, protection, OFFSET],
        )
        .map_err(|_| "i: the pager could not map its own VMO")?;
    let space = side.process.space();
    let writable = |page: u64| {
        mm::flags_in(space.root_table(), MAP_AT + page * PAGE_SIZE).is_some_and(|flags| flags.write)
    };
    let write = |page: u64| {
        space
            .fault(MAP_AT + page * PAGE_SIZE, Access::WRITE)
            .map_err(|_| "i: a write to a supplied page faulted")
    };
    let read = space
        .fault(MAP_AT, Access::READ)
        .map_err(|_| "i: a read of a supplied page faulted");
    read?;
    if writable(0) {
        return Err("i: a Clean page was mapped writable");
    }
    write(0)?;
    write(1)?;
    if !served.quiet() || served.vmo.1.page_state(0) != PageState::Dirty || !writable(0) {
        return Err(
            "i: the kernel's own dirty mark asked the pager, marked nothing, or left the page read-only",
        );
    }
    let _ = served
        .op(PAGER_OP_WRITEBACK_BEGIN, 0, 2, 0, 0)
        .map_err(|_| "i: pager_op WRITEBACK_BEGIN was refused")?;
    if writable(0) || served.vmo.1.page_state(0) != PageState::AwaitingClean {
        return Err("i: a write-back began and the page was still mapped writable");
    }
    write(1)?;
    let _ = served
        .op(PAGER_OP_WRITEBACK_END, 0, 2, 0, 0)
        .map_err(|_| "i: pager_op WRITEBACK_END was refused")?;
    if served.vmo.1.page_state(0) != PageState::Clean
        || served.vmo.1.page_state(1) != PageState::Dirty
    {
        return Err(
            "i: a write-back's end cleaned a page written since it began, or not the other",
        );
    }
    let _ = served
        .op(PAGER_OP_DECOMMIT, 0, 2, 0, 0)
        .map_err(|_| "i: pager_op DECOMMIT was refused")?;
    if served.vmo.1.page_state(0) != PageState::Absent
        || served.vmo.1.page_state(1) != PageState::Dirty
    {
        return Err("i: a decommit took a Dirty page, or left the Clean one");
    }
    Ok(())
}

/// Case j: each refusal, each changing nothing.
///
/// Verifies: L.object.228
fn case_j_supply_refused() -> Result<u32, &'static str> {
    let served = Served::new(0, 0)?;
    let side = &served.side;
    let aux = side.handle(nr::VMO_CREATE, &[2 * PAGE_SIZE], "vmo_create failed")?;
    let pager = served.pager()?;
    let mut refused = 0;
    let mut expect = |args: [u64; 4], spec: [u64; 4], wanted: Errno, what| {
        put_words(side, SPEC, &spec)?;
        if side.call(nr::PAGER_SUPPLY, &args) != Err(wanted) {
            return Err(what);
        }
        refused += 1;
        Ok(())
    };
    let good = [reg(pager), reg(served.vmo.0), reg(aux), SPEC];
    expect(
        good,
        [0, PAGE_SIZE, 0, 0],
        status::BAD_STATE,
        "j: an uncommitted aux page was supplied",
    )?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(aux), BYTES, PAGE_SIZE, OFFSET])
        .map_err(|_| "j: vmo_write into the aux VMO failed")?;
    let aux_object = vmo_of(side, aux)?;
    let held = aux_object
        .hold(0, 1)
        .map_err(|_| "j: the aux page could not be held")?;
    expect(
        good,
        [0, PAGE_SIZE, 0, 0],
        status::BAD_STATE,
        "j: a held aux page was supplied",
    )?;
    drop(held);
    expect(
        good,
        [0, PAGE_SIZE, PAGE_SIZE, 0],
        status::BAD_STATE,
        "j: an aux range with an absent page was supplied",
    )?;
    expect(
        good,
        [1, PAGE_SIZE, 0, 0],
        status::INVALID_ARGS,
        "j: an unaligned supply was taken",
    )?;
    expect(
        good,
        [PAGES * PAGE_SIZE, PAGE_SIZE, 0, 0],
        status::INVALID_ARGS,
        "j: a supply past the VMO was taken",
    )?;
    expect(
        good,
        [0, PAGE_SIZE, 0, 1],
        status::INVALID_ARGS,
        "j: a supply with an option was taken",
    )?;
    let read_only = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(aux), u64::from(Rights::READ.0)],
        "handle_duplicate failed",
    )?;
    expect(
        [reg(pager), reg(served.vmo.0), reg(read_only), SPEC],
        [0, PAGE_SIZE, 0, 0],
        status::ACCESS_DENIED,
        "j: an aux VMO without WRITE was taken",
    )?;
    let other = Served::new(0, 0)?;
    let foreign = side
        .process
        .with_handles(|table| table.insert(Object::Vmo(Arc::clone(&other.vmo.1)), Rights::VMO))
        .map_err(|_| "j: no room for a VMO")?;
    expect(
        [reg(pager), reg(foreign), reg(aux), SPEC],
        [0, PAGE_SIZE, 0, 0],
        status::BAD_STATE,
        "j: another pager's VMO was supplied",
    )?;
    expect(
        [reg(pager), reg(served.vmo.0), reg(served.vmo.0), SPEC],
        [0, PAGE_SIZE, 0, 0],
        status::BAD_STATE,
        "j: a pager VMO was taken as aux",
    )?;
    if served.vmo.1.page_state(0) != PageState::Absent || aux_object.page(0).is_none() {
        return Err("j: a refused supply changed a page");
    }
    Ok(refused)
}

/// Case k.
///
/// Verifies: L.object.231
fn case_k_no_pager() -> Result<u32, &'static str> {
    let served = Served::new(0, 0)?;
    let side = &served.side;
    side.put(OFFSET, &0_u64.to_ne_bytes())?;
    let protection = u64::from(MAP_READ | MAP_NO_PAGER);
    if side.call(
        nr::VMO_MAP,
        &[reg(served.vmo.0), MAP_AT, PAGE_SIZE, protection, OFFSET],
    ) != Err(status::WRONG_TYPE)
    {
        return Err("k: vmo_map with MAP_NO_PAGER mapped a pager's VMO");
    }
    let plain = side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")?;
    let _ = side
        .call(
            nr::VMO_MAP,
            &[reg(plain), MAP_AT, PAGE_SIZE, protection, OFFSET],
        )
        .map_err(|_| "k: vmo_map with MAP_NO_PAGER refused an anonymous VMO")?;
    Ok(1)
}

/// Case m.
///
/// Verifies: L.user.153
fn case_m_present_skipped() -> Result<(), &'static str> {
    let served = Served::new(0, 0)?;
    let _ = served.supply(1, 1)?;
    let before = served.vmo.1.page(1);
    let aux = served.supply(0, 2)?;
    if served.vmo.1.page(1) != before || aux.page(1).is_none() || aux.page(0).is_some() {
        return Err("m: a supply over a present page replaced it, or moved its aux page");
    }
    Ok(())
}

/// Case n.
///
/// Verifies: L.object.226, L.object.227, L.object.229
fn case_n_decoding() -> Result<u32, &'static str> {
    let served = Served::new(0, 0)?;
    let side = &served.side;
    let mut refused = 0;
    let mut count = |answer: Result<usize, Errno>, wanted: Errno, what| {
        if answer != Err(wanted) {
            return Err(what);
        }
        refused += 1;
        Ok(())
    };
    side.put(SPEC, &0_u64.to_ne_bytes())?;
    count(
        side.call(nr::PAGER_CREATE, &[reg(served.port.0), SPEC, 1]),
        status::INVALID_ARGS,
        "n: pager_create took an option",
    )?;
    count(
        side.call(nr::PAGER_CREATE, &[reg(served.vmo.0), SPEC, 0]),
        status::WRONG_TYPE,
        "n: pager_create took a VMO as its port",
    )?;
    let pager = served.pager()?;
    count(
        side.call(
            nr::HANDLE_DUPLICATE,
            &[reg(pager), u64::from(Rights::PAGER.0)],
        ),
        status::ACCESS_DENIED,
        "n: a pager handle was duplicated",
    )?;
    put_words(side, SPEC, &[1, PAGES, 1 << 2])?;
    count(
        side.call(nr::PAGER_CREATE_VMO, &[reg(pager), SPEC]),
        status::INVALID_ARGS,
        "n: pager_create_vmo took an unknown option",
    )?;
    count(
        served.op(9, 0, 1, 0, 0),
        status::INVALID_ARGS,
        "n: pager_op took an unknown operation",
    )?;
    count(
        served.op(PAGER_OP_DIRTY, 0, 1, 0, 0),
        status::BAD_STATE,
        "n: pager_op DIRTY took an absent page",
    )?;
    count(
        served.op(PAGER_OP_FAIL, 0, 1, 7, 0),
        status::INVALID_ARGS,
        "n: pager_op FAIL took an errno it does not carry",
    )?;
    if pager::clamp_wait(0) != PAGER_WAIT_MAX_NS
        || pager::clamp_wait(1) != PAGER_WAIT_MIN_NS
        || served.core.wait_ns() != PAGER_WAIT_MAX_NS
    {
        return Err("n: a pager's bound was not clamped to its range, 0 the maximum");
    }
    Ok(refused)
}
