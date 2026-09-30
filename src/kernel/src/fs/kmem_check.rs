//! The kernel heap the Linux personality holds for a job, charged to the job,
//! proved at boot (certification finding F-37, work order W-15).
//!
//! For each kind of object a program can make and keep through the Linux
//! calls, a job with a small memory limit makes them until one is refused,
//! through the path a program's call takes. Each kind must be refused with
//! `ENOMEM` -- not with the machine's memory running out, and not by some
//! other ceiling first -- with the job's memory never past its limit and its
//! kernel-memory count above zero; a sibling job with the same limit must
//! then make one; and once the objects are gone, both jobs must read zero,
//! and every quota slot the check took must be given back.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vfs::{Errno, Namespace, OpenFile, OpenFlags};
use ferrix_vma::VmaFlags;

use crate::fs;
use crate::fs::epoll::{self, Interest};
use crate::fs::socket::{Passed, SocketType};
use crate::object::job::Job;
use crate::object::quota::{self, Resource};
use crate::sched;
use crate::user::space::{AddressSpace, SpaceError};

/// The memory limit each kind is filled to.
pub(crate) const LIMIT: u64 = 32 * 1024;

/// More of any kind than the limit could pay for: a fill that reaches it
/// was not refused by the limit.
const MOST: usize = 100_000;

/// Where the files are made: a directory of the check's own in `/tmp`,
/// removed at the end with every name the fill looked up in it.
const DIRECTORY: &[u8] = b"/tmp/.kmem-check";

/// What the check saw, for the boot line: how many of each kind a job with
/// [`LIMIT`] bytes made before one was refused.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Files made in a tmpfs directory, each an inode, a name and a dentry.
    pub(crate) files: usize,
    /// Pipes, each two open files and a buffer.
    pub(crate) pipes: usize,
    /// `AF_UNIX` socket pairs.
    pub(crate) sockets: usize,
    /// Messages carrying a descriptor, in flight in a socket's queue.
    pub(crate) in_flight: usize,
    /// Registrations in an epoll set.
    pub(crate) registrations: usize,
    /// Eventfds.
    pub(crate) eventfds: usize,
    /// Regions one mapping was split into by `mprotect`.
    pub(crate) regions: usize,
    /// Record locks one open file description holds, on every other byte.
    pub(crate) locks: usize,
    /// System V semaphore sets of one semaphore each.
    pub(crate) sets: usize,
    /// Mount namespaces, each a copy of one holding [`TREE`] mounts, as
    /// `unshare(CLONE_NEWNS)` copies them (`docs/NAMESPACES.md` §5).
    pub(crate) namespaces: usize,
    /// User namespaces, each a level-1 child of the first.
    pub(crate) user_namespaces: usize,
    /// Network namespaces, each made as `unshare(CLONE_NEWNET)` makes one.
    pub(crate) network_namespaces: usize,
    /// Veth pairs, each between two namespaces made with it.
    pub(crate) veth_pairs: usize,
    /// Routes added to a network namespace by netlink.
    pub(crate) routes: usize,
}

/// How many mounts the namespace the mount namespaces are copied from
/// holds, its root included.
pub(crate) const TREE: usize = 64;

/// Run every kind.
///
/// # Errors
///
/// Which property failed, for which kind.
///
/// Verifies: L.object.56, H.QUOTA.7
pub(crate) fn run() -> Result<Report, &'static str> {
    let slots = quota::live_slots();
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.mkdir(&ctx, None, DIRECTORY, 0o755)
        .map_err(|_| "kmem: no directory for the files")?;
    let mut report = Report::default();
    {
        let tree = Job::new_root().map_err(|_| "kmem: no memory for a job")?;
        report.files = files(&tree)?;
        report.pipes = kind(&tree, "pipes", |_| fs::pipe::new_pipe(true, (0, 0)))?;
        report.sockets = sockets(&tree)?;
        report.in_flight = in_flight(&tree)?;
        report.registrations = registrations(&tree)?;
        report.eventfds = kind(&tree, "eventfds", |_| fs::eventfd::create(0, false, true))?;
        report.regions = regions(&tree)?;
        report.locks = locks(&tree)?;
        report.sets = kind(&tree, "semaphore sets", |_| {
            crate::syscall::sem_check::private_set(1, 0o600)
        })?;
        report.namespaces = namespaces(&tree)?;
        report.user_namespaces = user_namespaces(&tree)?;
        report.network_namespaces = network_namespaces(&tree)?;
        report.veth_pairs = veth_pairs(&tree)?;
        report.routes = routes(&tree)?;
        if Resource::ALL
            .iter()
            .any(|&resource| tree.usage(resource).is_none_or(|usage| usage.used != 0))
        {
            return Err("kmem: a job tree the checks emptied still holds something");
        }
    }
    ns.rmdir(&ctx, None, DIRECTORY)
        .map_err(|_| "kmem: the files' directory would not go")?;
    if quota::live_slots() != slots {
        return Err("kmem: the checks' jobs are gone and their quota slots are not");
    }
    Ok(report)
}

/// What `job` holds of `resource`.
fn used(job: &Job, resource: Resource) -> u64 {
    job.usage(resource).map_or(0, |usage| usage.used)
}

/// Run `work` charged to `job`, as a task of it would be.
fn as_task_of<T>(job: &Job, work: impl FnOnce() -> T) -> T {
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let done = work();
    sched::set_current_group(own);
    done
}

/// Make as many as the limit allows, and the refusal that stopped it.
fn fill<T>(mut make: impl FnMut(usize) -> Result<T, Errno>) -> (Vec<T>, Option<Errno>) {
    let mut held = Vec::new();
    while held.len() < MOST {
        match make(held.len()) {
            Ok(one) => held.push(one),
            Err(errno) => return (held, Some(errno)),
        }
    }
    (held, None)
}

/// Fill a job with `make`'s objects to its limit, see a sibling make one,
/// drop them all, run `clean`, and see both jobs empty. How many the job
/// made.
fn fill_and_empty<T>(
    tree: &Arc<Job>,
    name: &'static str,
    mut make: impl FnMut(usize) -> Result<T, Errno>,
    clean: impl FnOnce(usize),
) -> Result<usize, &'static str> {
    let job = tree
        .new_child()
        .map_err(|_| "kmem: a job refused a child")?;
    let sibling = tree
        .new_child()
        .map_err(|_| "kmem: a job refused a child")?;
    let _ = job.set_limit(Resource::Memory, LIMIT);
    let _ = sibling.set_limit(Resource::Memory, LIMIT);
    let (held, refused) = as_task_of(&job, || fill(&mut make));
    let made = held.len();
    let outcome = judge(&job, refused, made);
    let other = as_task_of(&sibling, || make(made));
    let sibling_charged = used(&sibling, Resource::Kernel) != 0;
    let other_made = other.is_ok();
    drop(other);
    drop(held);
    clean(made.saturating_add(1));
    if let Err(problem) = outcome {
        crate::console::println!("  kmem     {name}: {problem} after {made}");
        return Err(problem);
    }
    if !other_made || !sibling_charged {
        crate::console::println!("  kmem     {name}: the sibling made none, or was not charged");
        return Err("kmem: a job at its memory limit held back its sibling");
    }
    for held in [&job, &sibling] {
        if used(held, Resource::Memory) != 0 || used(held, Resource::Kernel) != 0 {
            crate::console::println!(
                "  kmem     {name}: {} bytes still charged, {} of them heap",
                used(held, Resource::Memory),
                used(held, Resource::Kernel)
            );
            return Err("kmem: objects gone and their heap still charged to their job");
        }
    }
    Ok(made)
}

/// [`fill_and_empty`] with nothing to clean.
fn kind<T>(
    tree: &Arc<Job>,
    name: &'static str,
    make: impl FnMut(usize) -> Result<T, Errno>,
) -> Result<usize, &'static str> {
    fill_and_empty(tree, name, make, |_| {})
}

/// Whether a job's fill stopped where its limit says: refused `ENOMEM`,
/// within its limit, with heap charged and the refusal counted.
fn judge(job: &Job, refused: Option<Errno>, made: usize) -> Result<(), &'static str> {
    match refused {
        None => return Err("kmem: a job made more than its limit could hold"),
        Some(Errno::ENOMEM) => {}
        Some(_) => return Err("kmem: a fill was refused by something other than its limit"),
    }
    if made == 0 {
        return Err("kmem: a job made none before its limit refused it");
    }
    if used(job, Resource::Memory) > LIMIT {
        return Err("kmem: a job's memory went past its limit");
    }
    if used(job, Resource::Kernel) == 0 {
        return Err("kmem: a job's objects were not charged to it as kernel memory");
    }
    if job.usage(Resource::Memory).map(|usage| usage.refused) == Some(0) {
        return Err("kmem: a refusal was not counted as memory.events counts it");
    }
    Ok(())
}

/// Files in a tmpfs directory, each made and closed, so what is held is the
/// inode, its name and its dentry.
fn files(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let flags = OpenFlags {
        read: true,
        write: true,
        create: true,
        ..OpenFlags::default()
    };
    let path = |at: usize| format!("{}/f{at}", core::str::from_utf8(DIRECTORY).unwrap_or(""));
    let made = fill_and_empty(
        tree,
        "files",
        |at| {
            ns.open(&ctx, None, path(at).as_bytes(), &flags, 0o644)
                .map(drop)
        },
        |count| {
            for at in 0..count {
                let _ = ns.unlink(&ctx, None, path(at).as_bytes());
            }
        },
    )?;
    Ok(made)
}

/// User namespaces, made as `unshare(CLONE_NEWUSER)` makes them: each is
/// charged to the job asking, refused `ENOMEM` at its limit, and gives its
/// heap back when it ends (`docs/NAMESPACES.md` §5).
fn user_namespaces(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let creator = crate::syscall::credentials::Credentials::root();
    kind(tree, "user namespaces", |_| {
        crate::syscall::userns::create(&creator)
    })
}

/// Network namespaces, made as `unshare(CLONE_NEWNET)` makes them: each is
/// charged to the job asking, for itself and not less than the structure it is,
/// and refused `ENOMEM` at the limit (`docs/NETNS.md` section 5).
fn network_namespaces(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let owner = Arc::clone(crate::syscall::userns::first());
    let made = kind(tree, "network namespaces", |_| {
        crate::net::namespace::create(Arc::clone(&owner))
    })?;
    // Every namespace is at least the structure it is: a limit that held more
    // than this many would not be charging for it.
    let least = size_of::<crate::net::NetNamespace>() as u64;
    if (made as u64).saturating_mul(least) > LIMIT {
        return Err("kmem: a network namespace was not charged to its job for itself");
    }
    Ok(made)
}

/// Veth pairs, each between two namespaces of its own: the pair's record is
/// charged to the job that makes it on top of the interfaces the tables pay
/// for, and the fill ends at the job's limit.
fn veth_pairs(tree: &Arc<Job>) -> Result<usize, &'static str> {
    use crate::net::{namespace, veth};
    let owner = Arc::clone(crate::syscall::userns::first());
    // One pair on its own: it costs its record and two interfaces, not less.
    let job = tree
        .new_child()
        .map_err(|_| "kmem: a job refused a child")?;
    let (one, two) = as_task_of(&job, || {
        Ok::<_, Errno>((
            namespace::create(Arc::clone(&owner))?,
            namespace::create(Arc::clone(&owner))?,
        ))
    })
    .map_err(|_| "kmem: a job could not make two network namespaces")?;
    let before = used(&job, Resource::Kernel);
    let _ = as_task_of(&job, || veth::create((&one, None), (&two, None)))
        .map_err(|_| "kmem: a job could not make a veth pair")?;
    let cost = used(&job, Resource::Kernel).saturating_sub(before);
    let least = veth::record_cost() + 2 * size_of::<ferrix_net::Interface>();
    drop((one, two));
    if cost < least as u64 {
        return Err("kmem: a veth pair was not charged to its job for its record and ends");
    }
    if used(&job, Resource::Kernel) != 0 || used(&job, Resource::Memory) != 0 {
        return Err("kmem: a veth pair's namespaces are gone and their heap is still charged");
    }
    kind(tree, "veth pairs", |_| {
        let one = namespace::create(Arc::clone(&owner))?;
        let two = namespace::create(Arc::clone(&owner))?;
        let _ = veth::create((&one, None), (&two, None))?;
        Ok((one, two))
    })
}

/// Routes added to a network namespace by netlink: what they add to the
/// namespace's tables is charged to the job that made it, and the adding ends
/// at the job's limit, not at the namespace's own ceiling.
fn routes(tree: &Arc<Job>) -> Result<usize, &'static str> {
    use alloc::sync::Weak;
    let owner = Arc::clone(crate::syscall::userns::first());
    // One namespace and one socket for each job, held weakly here so that the
    // fill's own list is what keeps them and they go with it.
    let mut homes: Vec<(u32, Weak<crate::net::netlink::NetlinkSocket>)> = Vec::new();
    kind(tree, "routes in a network namespace", |n| {
        let group = sched::running_group();
        let socket = match homes
            .iter()
            .find(|(each, _)| *each == group)
            .and_then(|(_, weak)| weak.upgrade())
        {
            Some(socket) => socket,
            None => {
                let fresh = crate::net::namespace::create(Arc::clone(&owner))?;
                let socket = super::netns_check::fill_socket(&fresh)?;
                homes.push((group, Arc::downgrade(&socket)));
                socket
            }
        };
        let added = super::netns_check::route_for_fill(&socket, n);
        // What the tables hold is what the job is charged for, also when the
        // request was refused afterwards: a route that was added and not paid
        // for is not the limit's doing.
        if !socket.namespace().tables_cover() {
            return Err(Errno::EFAULT);
        }
        added?;
        Ok(socket)
    })
}

/// Mount namespaces, each a copy of one of [`TREE`] mounts: the namespace
/// and every mount copied are charged to the job asking, and a copy refused
/// part way gives back what it had made.
fn namespaces(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let source = Namespace::new(fs::kernel_tmpfs(), Arc::new(crate::sync::SchedParker));
    let ctx = source.context();
    // `/` and the bottom mount under it are two of them.
    for at in 2..TREE {
        let name = format!("/m{at}");
        let place = source
            .mkdir(&ctx, None, name.as_bytes(), 0o755)
            .and_then(|()| source.resolve(&ctx, None, name.as_bytes(), true))
            .map_err(|_| "kmem: no place for a mount to copy")?;
        let filesystem = fs::new_tmpfs().map_err(|_| "kmem: no tmpfs to mount")?;
        let _ = source
            .mount(filesystem, &place)
            .map_err(|_| "kmem: a mount to copy was refused")?;
    }
    if source.mounts().len() != TREE {
        return Err("kmem: the namespace to copy does not hold its mounts");
    }
    let made = kind(tree, "mount namespaces", |_| source.copy(&mut []))?;
    // Every mount a copy makes is charged, not only the namespace: at least
    // a small object's worth each, or the limit held more copies than it
    // could pay for.
    const LEAST_A_MOUNT_COSTS: u64 = 64;
    let charged = (made as u64)
        .saturating_mul(TREE as u64)
        .saturating_mul(LEAST_A_MOUNT_COSTS);
    if charged > LIMIT {
        return Err("kmem: a mount namespace's copied mounts were not charged to its job");
    }
    Ok(made)
}

/// `AF_UNIX` socket pairs, made by a process of the check's own.
fn sockets(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let maker =
        crate::syscall::process::new_for_check().map_err(|_| "kmem: no process for sockets")?;
    let made = kind(tree, "sockets", |_| {
        fs::socket::new_pair(SocketType::Stream, true, &maker)
    })?;
    crate::syscall::process::kill(&maker, crate::object::job::KILLED_STATUS);
    Ok(made)
}

/// Messages carrying a descriptor, sent into a socket nobody's job made:
/// what they cost is the sender's, as Linux charges it, and the file they
/// carry stays its opener's.
fn in_flight(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let maker =
        crate::syscall::process::new_for_check().map_err(|_| "kmem: no process for sockets")?;
    let (end, far) = fs::socket::new_pair(SocketType::Stream, true, &maker)
        .map_err(|_| "kmem: no socket pair for descriptors in flight")?;
    let socket = fs::socket::of(&end).ok_or("kmem: a socket pair's end was not a socket")?;
    let carried = fs::eventfd::create(0, false, true).map_err(|_| "kmem: no file to send")?;
    let receiver = fs::socket::of(&far).ok_or("kmem: a socket pair's end was not a socket")?;
    let made = fill_and_empty(
        tree,
        "descriptors in flight",
        |_| {
            let passed = Passed::new(vec![Arc::clone(&carried)])?;
            socket.send_passing(b"x", 0, true, Some(passed))
        },
        // Read, and the files they carried closed, as a `read` that cannot
        // install them does: every message's charge goes with it.
        |_| {
            let mut byte = [0_u8; 1];
            while receiver.recv(&mut byte, 0, true).is_ok() {}
        },
    )?;
    drop((far, receiver, socket, end, carried));
    crate::syscall::process::kill(&maker, crate::object::job::KILLED_STATUS);
    Ok(made)
}

/// Registrations of one eventfd under ever more numbers in a set nobody's
/// job made: each is charged to the job that registered it.
fn registrations(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let set = epoll::create().map_err(|_| "kmem: no epoll set")?;
    let epoll = epoll::of(&set).ok_or("kmem: an epoll set was not one")?;
    let watched: Arc<OpenFile> =
        fs::eventfd::create(0, false, true).map_err(|_| "kmem: no file to watch")?;
    let interest = Interest { events: 0, data: 0 };
    let made = fill_and_empty(
        tree,
        "registrations",
        |at| {
            let fd = i32::try_from(at).map_err(|_| Errno::EINVAL)?;
            epoll.add(fd, &watched, interest)?;
            Ok(fd)
        },
        // `EPOLL_CTL_DEL` of each, and each charge with it.
        |count| {
            for at in 0..count {
                let _ = i32::try_from(at).map(|fd| epoll.remove(fd, &watched));
            }
        },
    );
    drop((epoll, set, watched));
    made
}

/// Record locks on every other byte of one file, by one open description:
/// each a record the job that set it pays for, refused `ENOLCK` as Linux
/// refuses a lock it has no memory for.
fn locks(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let file = fs::eventfd::create(0, false, true).map_err(|_| "kmem: no file to lock")?;
    let made = fill_and_empty(
        tree,
        "record locks",
        |at| {
            crate::syscall::flock::check::lock_byte(&file, at).map_err(|errno| {
                // What a job at its limit is told, and the check's ENOMEM.
                if errno == Errno::ENOLCK {
                    Errno::ENOMEM
                } else {
                    errno
                }
            })
        },
        |_| crate::syscall::flock::check::unlock_all(&file),
    )?;
    drop(file);
    Ok(made)
}

/// One mapping split into a region a page by `mprotect` of every other page,
/// in a space the job made: the regions are the space's job's.
fn regions(tree: &Arc<Job>) -> Result<usize, &'static str> {
    const BASE: u64 = 0x40_0000;
    const PAGES: u64 = 16 * 1024;
    let job = tree
        .new_child()
        .map_err(|_| "kmem: a job refused a child")?;
    let sibling = tree
        .new_child()
        .map_err(|_| "kmem: a job refused a child")?;
    let _ = job.set_limit(Resource::Memory, LIMIT);
    let _ = sibling.set_limit(Resource::Memory, LIMIT);
    let split = |job: &Job, most: u64| {
        as_task_of(job, || {
            let space = AddressSpace::new().map_err(|_| "kmem: no address space")?;
            let _ = space
                .map_anonymous(BASE, PAGES * PAGE_SIZE, VmaFlags::READ_WRITE)
                .map_err(|_| "kmem: no mapping to split")?;
            let mut splits = 0;
            let refused = loop {
                if splits >= most {
                    break None;
                }
                let at = BASE + (2 * splits + 1) * PAGE_SIZE;
                match space.protect(at, PAGE_SIZE, VmaFlags::READ) {
                    Ok(()) => splits += 1,
                    Err(error) => break Some(error),
                }
            };
            Ok::<_, &'static str>((space, splits, refused))
        })
    };
    let (space, made, refused) = split(&job, PAGES / 2)?;
    if !matches!(refused, Some(SpaceError::OutOfMemory)) {
        return Err("kmem: splitting a mapping was not refused at its job's limit");
    }
    judge(
        &job,
        Some(Errno::ENOMEM),
        usize::try_from(made).unwrap_or(0),
    )?;
    let (other, one, _) = split(&sibling, 1)?;
    if one != 1 || used(&sibling, Resource::Kernel) == 0 {
        return Err("kmem: a job at its memory limit held back its sibling's regions");
    }
    drop((space, other));
    if [&job, &sibling]
        .iter()
        .any(|held| used(held, Resource::Memory) != 0 || used(held, Resource::Kernel) != 0)
    {
        return Err("kmem: spaces gone and their regions still charged");
    }
    Ok(usize::try_from(made).unwrap_or(0))
}
