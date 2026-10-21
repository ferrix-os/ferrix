//! Charging that follows the client, proved at boot (`docs/microkernel/K7.md`
//! §3 and §4; the consultant's OK IF, ledger 2525).
//!
//! A job tree made for the check holds a client job C, a server job S and a
//! second client job D under one parent, a process in each, and channels
//! between the client and the server. Every `charge_ticket`, VMO, channel and
//! port call goes through the native dispatch as the process whose call it is
//! ([`Side`]), with this task charging the process's job as a task of it
//! would ([`as_task_of`]). The adoption is this task's: the server's calls are
//! made as a task of S, so this task plays the server's thread. The cases, in
//! the order the negative controls of K7.md §3 need:
//!
//! 1. a ticket names its issuer: `ADOPT` answers the issuing process's id and
//!    its job's, a second process sharing the client's end issues for its own
//!    job, and an adopted commit is charged to the issuer's job;
//! 2. refused adoptions: an id never issued, one ended, one issued on the
//!    adopting end, one issued on another channel -- each `BAD_STATE`, each
//!    audited, and each leaving the server charging its own job;
//! 3. an adopted call's VMO commits and new objects go to C, none to S, and
//!    C's kernel heap does not move; faults on S's own memory stay S's; at
//!    C's limit the commit is `NO_MEMORY` with no throttle and no kill; after
//!    `LEAVE` S charges itself;
//! 4. the end: after the server's `END`, the client's `END` and the client's
//!    close, the next call charges S without a `LEAVE`; ids only grow;
//! 5. the bound: 64 live tickets on one end, the 65th refused, room again
//!    after an `END`, and each ticket one of C's objects;
//! 6. the return: what the tickets charged stays C's after they end, and
//!    every counter is back at zero once the server lets go of it and the
//!    processes and jobs are gone.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{MAX_TICKETS, TICKET_ADOPT, TICKET_END, TICKET_ISSUE, TICKET_LEAVE};
use ferrix_vma::VmaFlags;

use super::Object;
use super::channel::Endpoint;
use super::check::{SCRATCH, Side, reg};
use super::job::Job;
use super::process::Host;
use super::quota::{self, Counter, Mark, Resource};
use crate::audit;
use crate::sched;

/// Where a call writes its answer: an id, or an issuer's two words.
const OUT: u64 = SCRATCH + 0x800;
/// Where `vmo_write`'s offset is.
const OFFSET: u64 = SCRATCH + 0x840;
/// The byte `vmo_write` copies, one a page.
const BYTE: u64 = SCRATCH + 0x880;
/// Where `channel_create` writes its pair.
const PAIR: u64 = SCRATCH + 0x8C0;
/// Where the server's own fresh memory is, for a call to fault in.
const OWN: u64 = 0x5000_0000;
/// Pages an adopted call commits.
const PAGES: u64 = 4;

/// What the boot line counts.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Refused calls, each audited.
    pub(crate) refused: u32,
    /// Pages an adopted call committed to the client.
    pub(crate) pages: u64,
    /// Objects an adopted call made for the client.
    pub(crate) objects: u64,
    /// Pages the server faulted in under an adoption, charged to itself.
    pub(crate) own_pages: u64,
    /// Tickets live on one end at the bound.
    pub(crate) bound: usize,
}

/// One check's world: the jobs, the processes, the channels.
struct World {
    /// The tree the check's jobs hang from.
    tree: Arc<Job>,
    /// The parent of C, S and D.
    parent: Arc<Job>,
    /// The client's job.
    client_job: Arc<Job>,
    /// The server's job.
    server_job: Arc<Job>,
    /// The second client's job.
    other_job: Arc<Job>,
    /// The client, in C.
    client: Side,
    /// The server, in S.
    server: Side,
    /// The second client, in D, sharing the client's end.
    other: Side,
}

/// Run every case.
///
/// # Errors
///
/// Which property failed.
///
/// Verifies: H.QUOTA.14
pub(crate) fn run() -> Result<Report, &'static str> {
    let slots = quota::live_slots();
    let mut report = Report::default();
    {
        let world = World::new()?;
        let result = cases(&world, &mut report);
        // Whatever the cases left: no adoption on this task, every handle
        // closed, every process gone, and only then the jobs looked at.
        super::ticket::leave();
        let World {
            tree,
            parent,
            client_job,
            server_job,
            other_job,
            client,
            server,
            other,
        } = world;
        for side in [&client, &server, &other] {
            side.close_everything();
        }
        drop((client, server, other));
        result?;
        check_everything_came_back(&[&client_job, &server_job, &other_job, &parent, &tree])?;
    }
    if quota::live_slots() != slots {
        return Err("the checks' jobs are gone and their quota slots are not");
    }
    Ok(report)
}

/// The cases in order.
fn cases(world: &World, report: &mut Report) -> Result<(), &'static str> {
    let (client_end, server_end) = world.channel()?;
    let mut ids = Vec::new();
    check_the_issuer(world, client_end, server_end, &mut ids)?;
    report.refused = check_refused_adoptions(world, client_end, server_end, &mut ids)?;
    check_adopted_charges(world, client_end, server_end, report, &mut ids)?;
    check_the_end(world, client_end, server_end, &mut ids)?;
    report.bound = check_the_bound(world, &mut ids)?;
    if !ids.is_sorted_by(|earlier, later| earlier < later) {
        return Err("ticket ids did not only grow");
    }
    Ok(())
}

impl World {
    /// The jobs and processes, each process moved into its job.
    fn new() -> Result<World, &'static str> {
        let tree = Job::new_root().map_err(|_| "no memory for a job")?;
        let parent = tree.new_child().map_err(|_| "a job refused a child")?;
        let client_job = parent.new_child().map_err(|_| "a job refused a child")?;
        let server_job = parent.new_child().map_err(|_| "a job refused a child")?;
        let other_job = parent.new_child().map_err(|_| "a job refused a child")?;
        let member = |job: &Arc<Job>| -> Result<Side, &'static str> {
            let side = Side::new()?;
            side.process
                .move_new_to(job)
                .map_err(|_| "a job refused a process")?;
            // The scratch region's pages, touched now, so that no call's copy
            // faults them in under a ticket.
            side.put(SCRATCH, &[0; 0x900])?;
            Ok(side)
        };
        Ok(World {
            client: member(&client_job)?,
            server: member(&server_job)?,
            other: member(&other_job)?,
            tree,
            parent,
            client_job,
            server_job,
            other_job,
        })
    }

    /// A new channel: one end in the client and the second client (one end
    /// shared by both), the other in the server.
    fn channel(&self) -> Result<(Handle, Handle), &'static str> {
        let (near, far) = Endpoint::pair().map_err(|_| "no memory for a channel")?;
        let give = |side: &Side, end: &Arc<Endpoint>| {
            side.process
                .with_handles(|table| {
                    table.insert(Object::Channel(Arc::clone(end)), Rights::CHANNEL)
                })
                .map_err(|_| "no room for a channel end")
        };
        let client = give(&self.client, &near)?;
        let other = give(&self.other, &near)?;
        let server = give(&self.server, &far)?;
        if client != other {
            // The two clients' tables are alike so far, so the shared end has
            // the same value in both; the cases rely on it.
            return Err("the shared end has two values");
        }
        Ok((client, server))
    }
}

/// Run `work` charged to `job`, as a task of it would be.
fn as_task_of<T>(job: &Job, work: impl FnOnce() -> T) -> T {
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let done = work();
    sched::set_current_group(own);
    done
}

/// What `job` uses of `resource`.
fn used(job: &Job, resource: Resource) -> u64 {
    job.usage(resource).map_or(u64::MAX, |usage| usage.used)
}

/// Frames `job` is charged for, its kernel heap left out.
fn pages(job: &Job) -> u64 {
    used(job, Resource::Memory).saturating_sub(used(job, Resource::Kernel)) / PAGE_SIZE
}

/// `charge_ticket(op, end, id, OUT)` in `side`, the id split in two words.
fn ticket(side: &Side, op: u64, end: Handle, id: u64) -> Result<usize, Errno> {
    side.call(
        nr::CHARGE_TICKET,
        &[op, reg(end), id & u64::from(u32::MAX), id >> 32, OUT],
    )
}

/// Issue a ticket on `end` in `side`, as a task of `job`: its id.
fn issue(side: &Side, job: &Job, end: Handle) -> Result<u64, &'static str> {
    let _ = as_task_of(job, || ticket(side, TICKET_ISSUE, end, 0))
        .map_err(|_| "an issue was refused")?;
    let bytes = side.get(OUT, 8)?;
    <[u8; 8]>::try_from(bytes.as_slice())
        .map(u64::from_ne_bytes)
        .map_err(|_| "a short read")
}

/// Adopt `id` through `end` in the server: the issuer's process and job ids.
fn adopt(world: &World, end: Handle, id: u64) -> Result<(u64, u64), Errno> {
    let _ = as_task_of(&world.server_job, || {
        ticket(&world.server, TICKET_ADOPT, end, id)
    })?;
    let bytes = world.server.get(OUT, 16).map_err(|_| status::FAULT)?;
    let word = |at: usize| {
        let mut raw = [0_u8; 8];
        if let Some(from) = bytes.get(at..at + 8) {
            raw.copy_from_slice(from);
        }
        u64::from_ne_bytes(raw)
    };
    Ok((word(0), word(8)))
}

/// Make a VMO of `pages` pages in the server, as a task of S, and commit
/// each page with a one-byte `vmo_write`: the VMO's handle.
fn commit(world: &World, pages: u64) -> Result<Handle, Errno> {
    let server = &world.server;
    as_task_of(&world.server_job, || {
        let vmo = server.call(nr::VMO_CREATE, &[pages * PAGE_SIZE])?;
        let vmo = Handle(u32::try_from(vmo).map_err(|_| status::INVALID_ARGS)?);
        for page in 0..pages {
            server
                .put(OFFSET, &(page * PAGE_SIZE).to_ne_bytes())
                .map_err(|_| status::FAULT)?;
            let _ = server.call(nr::VMO_WRITE, &[reg(vmo), BYTE, 1, OFFSET])?;
        }
        Ok(vmo)
    })
}

/// Close `handle` in `side`.
fn close(side: &Side, handle: Handle) {
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
}

/// The `TICKET` records the refusal ring holds.
fn ticket_records() -> u64 {
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
            .filter(|record| record.is(audit::TICKET))
            .count() as u64;
        from = read.next;
    }
}

/// Case 1: a ticket names the process that issued it and that process's
/// job, read by the kernel; a process sharing the end issues for its own
/// job; and an adopted commit is charged to the issuer's job.
///
/// Verifies: L.object.207
fn check_the_issuer(
    world: &World,
    client_end: Handle,
    server_end: Handle,
    ids: &mut Vec<u64>,
) -> Result<(), &'static str> {
    let id = issue(&world.client, &world.client_job, client_end)?;
    ids.push(id);
    let issuer = adopt(world, server_end, id).map_err(|_| "a live ticket was not adopted")?;
    let client = Host::core(&*world.client.process);
    if issuer != (client.id(), world.client_job.id()) {
        return Err("a ticket did not name its issuer's own process and job");
    }
    let before = pages(&world.client_job);
    let vmo = commit(world, 1).map_err(|_| "an adopted commit within every limit was refused")?;
    if pages(&world.client_job) != before + 1 {
        return Err("a ticket's charge did not go to the job that issued it");
    }
    close(&world.server, vmo);
    super::ticket::leave();

    let shared = issue(&world.other, &world.other_job, client_end)?;
    ids.push(shared);
    let other = Host::core(&*world.other.process);
    if adopt(world, server_end, shared) != Ok((other.id(), world.other_job.id())) {
        return Err("a ticket issued on a shared end did not name its own issuer's job");
    }
    super::ticket::leave();
    let _ = ticket(&world.server, TICKET_END, server_end, shared);
    let _ = ticket(&world.server, TICKET_END, server_end, id);
    Ok(())
}

/// Whether the server, with whatever adoption it has, charges its own job
/// for a commit, and the client's job nothing.
fn charges_itself(world: &World) -> Result<bool, &'static str> {
    let (client, server) = (pages(&world.client_job), pages(&world.server_job));
    let vmo = commit(world, 1).map_err(|_| "a server's own commit was refused")?;
    let itself = pages(&world.server_job) == server + 1 && pages(&world.client_job) == client;
    close(&world.server, vmo);
    Ok(itself)
}

/// Case 2: an id never issued, one ended, one issued on the adopting end and
/// one issued on another channel are each refused `BAD_STATE`, audited, and
/// leave the server charging its own job.
///
/// Verifies: L.object.208
fn check_refused_adoptions(
    world: &World,
    client_end: Handle,
    server_end: Handle,
    ids: &mut Vec<u64>,
) -> Result<u32, &'static str> {
    let records = ticket_records();
    let mut refused = 0;
    let mut refuse = |id: u64, end: Handle, why: &'static str| -> Result<(), &'static str> {
        if adopt(world, end, id) != Err(status::BAD_STATE) {
            return Err(why);
        }
        if !charges_itself(world)? {
            return Err("a refused adoption left the server charging another job");
        }
        refused += 1;
        Ok(())
    };
    refuse(u64::MAX / 2, server_end, "an id never issued was adopted")?;

    let ended = issue(&world.client, &world.client_job, client_end)?;
    ids.push(ended);
    let _ = as_task_of(&world.client_job, || {
        ticket(&world.client, TICKET_END, client_end, ended)
    })
    .map_err(|_| "a client could not end its own ticket")?;
    refuse(ended, server_end, "an ended ticket was adopted")?;

    let own = issue(&world.server, &world.server_job, server_end)?;
    ids.push(own);
    refuse(
        own,
        server_end,
        "a ticket was adopted through the end that issued it",
    )?;
    let _ = ticket(&world.server, TICKET_END, server_end, own);

    let (elsewhere_client, elsewhere_server) = world.channel()?;
    let elsewhere = issue(&world.client, &world.client_job, elsewhere_client)?;
    ids.push(elsewhere);
    refuse(
        elsewhere,
        server_end,
        "a ticket issued on another channel was adopted",
    )?;
    // Closing the other channel's client end ends its ticket.
    close(&world.client, elsewhere_client);
    close(&world.other, elsewhere_client);
    close(&world.server, elsewhere_server);

    // Four refusals, and the ends of `ended` and `own` (the third `END`, of a
    // ticket the close had already ended, is not made).
    if ticket_records() < records + u64::from(refused) + 2 {
        return Err("a ticket's refusal or end was not audited");
    }
    Ok(refused)
}

/// Case 3: an adopted call's VMO commits and new objects go to the client's
/// job and none to the server's; the client's heap does not move; faults on
/// the server's own memory stay the server's; at the client's limit the
/// commit is refused with no throttle and no kill; after `LEAVE` the server
/// charges itself.
///
/// Verifies: L.object.209
fn check_adopted_charges(
    world: &World,
    client_end: Handle,
    server_end: Handle,
    report: &mut Report,
    ids: &mut Vec<u64>,
) -> Result<(), &'static str> {
    let (client, server) = (&world.client_job, &world.server_job);
    let id = issue(&world.client, client, client_end)?;
    ids.push(id);
    let _ = adopt(world, server_end, id).map_err(|_| "a live ticket was not adopted")?;

    let objects = (
        used(client, Resource::Objects),
        used(server, Resource::Objects),
    );
    let frames = (pages(client), pages(server));
    let heap = used(client, Resource::Kernel);
    let vmo =
        commit(world, PAGES).map_err(|_| "an adopted commit within every limit was refused")?;
    let made = as_task_of(server, || -> Result<(), Errno> {
        let _ = world.server.call(nr::CHANNEL_CREATE, &[PAIR])?;
        let _ = world.server.call(nr::PORT_CREATE, &[])?;
        Ok(())
    });
    made.map_err(|_| "an adopted call within every limit was refused")?;
    if pages(client) != frames.0 + PAGES || pages(server) != frames.1 {
        return Err("an adopted call's VMO commit was not charged to the ticket's job");
    }
    // The VMO, the channel's two ends and the port.
    if used(client, Resource::Objects) != objects.0 + 4
        || used(server, Resource::Objects) != objects.1
    {
        return Err("an adopted call's new objects were not charged to the ticket's job");
    }
    if used(client, Resource::Kernel) != heap {
        return Err("an adopted call charged the server's kernel heap to the client");
    }
    report.pages = PAGES;
    report.objects = 4;

    // The server's own fresh memory, which a `vmo_read` into it faults in:
    // its frames and their page tables are the server's.
    let _ = world
        .server
        .process
        .space()
        .map_anonymous(OWN, 2 * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "no room for the server's own memory")?;
    let (client_now, server_now) = (pages(client), pages(server));
    let _ = as_task_of(server, || {
        world.server.put(OFFSET, &0_u64.to_ne_bytes())?;
        world
            .server
            .call(nr::VMO_READ, &[reg(vmo), OWN, 2 * PAGE_SIZE, OFFSET])
            .map_err(|_| "an adopted read into the server's own memory was refused")
    })?;
    if pages(client) != client_now || pages(server) < server_now + 2 {
        return Err("a fault on the server's own memory under a ticket was not the server's");
    }
    report.own_pages = pages(server) - server_now;

    // At the client's limit: the next page is refused, the server neither
    // slept on the client's account nor paid, and no task was killed.
    let _ = client.set_limit(
        Resource::Memory,
        used(client, Resource::Memory) + PAGE_SIZE / 2,
    );
    let _ = client.set_mark(Mark::High, 0);
    let _ = server.set_mark(Mark::High, 0);
    let highs = (client.counted(Counter::High), server.counted(Counter::High));
    let server_before = pages(server);
    let refused = commit(world, 1);
    let _ = client.set_limit(Resource::Memory, quota::UNLIMITED);
    let _ = client.set_mark(Mark::High, quota::UNLIMITED);
    let _ = server.set_mark(Mark::High, quota::UNLIMITED);
    if refused != Err(status::NO_MEMORY) {
        return Err("an adopted commit past the client's limit was not refused NO_MEMORY");
    }
    if (client.counted(Counter::High), server.counted(Counter::High)) != highs {
        return Err("an adopted call refused at the client's limit was throttled");
    }
    if pages(server) != server_before {
        return Err("an adopted call refused at the client's limit charged the server");
    }
    for side in [&world.client, &world.server, &world.other] {
        if Host::core(&*side.process).exit().is_terminated() {
            return Err("an adopted call refused at the client's limit killed a task");
        }
    }

    // LEAVE: the server charges itself again, and the ticket stays live.
    let _ = as_task_of(server, || {
        ticket(&world.server, TICKET_LEAVE, server_end, 0)
    })
    .map_err(|_| "LEAVE was refused")?;
    if !charges_itself(world)? {
        return Err("a server that left its ticket still charged the client");
    }
    // Ended, the ticket gives back nothing: the VMO it paid for is still the
    // client's until the server lets go of it (L.object.211).
    let held = pages(client);
    let _ = ticket(&world.server, TICKET_END, server_end, id);
    if pages(client) != held || held < frames.0 + PAGES {
        return Err("a ticket's end gave back what it charged");
    }
    close(&world.server, vmo);
    Ok(())
}

/// Case 4: after the server's `END`, the client's `END` and the client's
/// close, the server's next call charges its own job with no `LEAVE`.
///
/// Verifies: L.object.210
fn check_the_end(
    world: &World,
    client_end: Handle,
    server_end: Handle,
    ids: &mut Vec<u64>,
) -> Result<(), &'static str> {
    let by_server = issue(&world.client, &world.client_job, client_end)?;
    ids.push(by_server);
    let _ = adopt(world, server_end, by_server).map_err(|_| "a live ticket was not adopted")?;
    let _ = as_task_of(&world.server_job, || {
        ticket(&world.server, TICKET_END, server_end, by_server)
    })
    .map_err(|_| "a server could not end a live ticket")?;
    if !charges_itself(world)? {
        return Err("a call after the server's END was charged to the client");
    }

    let by_client = issue(&world.client, &world.client_job, client_end)?;
    ids.push(by_client);
    let _ = adopt(world, server_end, by_client).map_err(|_| "a live ticket was not adopted")?;
    let _ = as_task_of(&world.client_job, || {
        ticket(&world.client, TICKET_END, client_end, by_client)
    })
    .map_err(|_| "a client could not end a live ticket")?;
    if !charges_itself(world)? {
        return Err("a call after the client's END was charged to the client");
    }

    let (closing_client, closing_server) = world.channel()?;
    let by_close = issue(&world.client, &world.client_job, closing_client)?;
    ids.push(by_close);
    let _ = adopt(world, closing_server, by_close).map_err(|_| "a live ticket was not adopted")?;
    close(&world.client, closing_client);
    close(&world.other, closing_client);
    if !charges_itself(world)? {
        return Err("a call after the client's close was charged to the client");
    }
    close(&world.server, closing_server);
    super::ticket::leave();
    Ok(())
}

/// Case 5: 64 tickets live on one end, the 65th refused, room again after an
/// `END`, each ticket one of the issuer's objects.
///
/// Verifies: L.object.212
fn check_the_bound(world: &World, ids: &mut Vec<u64>) -> Result<usize, &'static str> {
    let (client_end, server_end) = world.channel()?;
    let objects = used(&world.client_job, Resource::Objects);
    let mut live = Vec::new();
    for _ in 0..MAX_TICKETS {
        live.push(issue(&world.client, &world.client_job, client_end)?);
    }
    if used(&world.client_job, Resource::Objects) != objects + MAX_TICKETS as u64 {
        return Err("a ticket was not one of its issuer's objects");
    }
    let past = as_task_of(&world.client_job, || {
        ticket(&world.client, TICKET_ISSUE, client_end, 0)
    });
    if past != Err(status::SHOULD_WAIT) {
        return Err("a ticket past the bound on one end was issued");
    }
    let first = live.first().copied().unwrap_or(0);
    let _ = as_task_of(&world.client_job, || {
        ticket(&world.client, TICKET_END, client_end, first)
    })
    .map_err(|_| "a client could not end a live ticket")?;
    live.push(issue(&world.client, &world.client_job, client_end)?);
    ids.extend(live);
    close(&world.client, client_end);
    close(&world.other, client_end);
    close(&world.server, server_end);
    if used(&world.client_job, Resource::Objects) != objects {
        return Err("a closed channel's tickets were still charged to their issuer");
    }
    Ok(MAX_TICKETS)
}

/// Case 6: what the tickets charged stayed the client's until it went, and
/// every job is empty now that the processes have closed everything.
///
/// Verifies: L.object.211
fn check_everything_came_back(jobs: &[&Arc<Job>]) -> Result<(), &'static str> {
    for job in jobs {
        if used(job, Resource::Objects) != 0 || pages(job) != 0 {
            return Err("what a ticket charged did not come back to the job it charged");
        }
    }
    Ok(())
}
