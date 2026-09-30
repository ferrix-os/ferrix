//! What each file under `/proc` says.
//!
//! The kernel's half of every file: gathering the numbers. The arranging of
//! them is `src/lib/fs/procfs`'s. Wherever a field has nothing true to put in it,
//! the comment at the place it is filled says so and says why, because a
//! plausible number with no source is the one kind of wrong answer nobody
//! goes looking for.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use ferrix_bootinfo::{Arch, PAGE_SIZE};
use ferrix_net::IpAddress;
use ferrix_procfs::filesystems::{self, Filesystem};
use ferrix_procfs::kstat::{self, CpuTimes, Kstat};
use ferrix_procfs::loadavg::{self as loadavg_text, Loadavg};
use ferrix_procfs::maps::{self, Mapping, Width};
use ferrix_procfs::meminfo::{self, Meminfo};
use ferrix_procfs::mounts::{self, Mount, MountInfo};
use ferrix_procfs::net as procfs_net;
use ferrix_procfs::partitions::{self, Partition};
use ferrix_procfs::stat::{self, Stat};
use ferrix_procfs::status::{self, State, Status};
use ferrix_procfs::sysctl;
use ferrix_vfs::fd::MAX_LIMIT;
use ferrix_vfs::{Errno, Location, MountFlags, Namespace, OpenFile, Result};

use super::{Kernel, ThreadOf};
use crate::arch;
use crate::fs;
use crate::fs::{block, devfs};
use crate::irq;
use crate::mm;
use crate::sched;
use crate::smp;
use crate::syscall::nsproxy;
use crate::syscall::process::{self, Process};
use crate::syscall::registry::PID_MAX;
use crate::syscall::system::{NAME_MAX, RELEASE, SYSNAME, VERSION};
use crate::syscall::time;
use crate::syscall::userns::{self, Kind as IdKind, UserNamespace};
use crate::user::space::Region;

/// Nanoseconds in a second.
const NANOS: u64 = 1_000_000_000;

/// Clock ticks a second, as `AT_CLKTCK` tells every program; `stat`'s times
/// are counted in these.
const CLOCK_TICKS: u64 = 100;

/// The signal a process's parent is sent when it exits: `SIGCHLD`.
const SIGCHLD: i32 = 17;

/// Append formatted text. Formatting into a vector cannot fail.
fn put(out: &mut Vec<u8>, arguments: fmt::Arguments<'_>) {
    struct Sink<'a>(&'a mut Vec<u8>);
    impl fmt::Write for Sink<'_> {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.0.extend_from_slice(text.as_bytes());
            Ok(())
        }
    }
    let _ = fmt::Write::write_fmt(&mut Sink(out), arguments);
}

// -- The top level ------------------------------------------------------------

/// `/proc/self`: the caller's pid. `ENOENT` from a kernel thread, which has
/// no process to name, as on Linux.
pub(super) fn self_link(_: &Kernel) -> Result<Vec<u8>> {
    let process = process::current().ok_or(Errno::ENOENT)?;
    let mut target = Vec::new();
    put(&mut target, format_args!("{}", process.pid()));
    Ok(target)
}

/// `/proc/cpuinfo`: a block per online processor.
///
/// Only what the kernel knows. Linux's x86-64 blocks carry the vendor, model
/// and a line of feature flags from `CPUID`, and its Arm blocks the `MIDR`
/// fields and `HWCAP` names; nothing here has read those, so nothing here
/// claims them. What is left is the number of blocks — which is what a C
/// library's `sysconf(_SC_NPROCESSORS_ONLN)` fallback counts — and each
/// processor's hardware identifier under the name Linux gives it, and on
/// x86-64 its `cpu MHz`, when the kernel has measured the TSC.
pub(super) fn cpuinfo(_: &Kernel) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let Some(topology) = smp::topology() else {
        return Ok(out);
    };
    for cpu in topology.cpus().iter().filter(|cpu| cpu.is_online()) {
        put(&mut out, format_args!("processor\t: {}\n", cpu.logical));
        match arch::ARCH {
            Arch::X86_64 => {
                // Linux's `cpu MHz`, the TSC's rate, which Steam's runtime
                // reads when there is no cpufreq and stops without. Only when
                // the kernel's counter is the TSC: then its rate was measured
                // against the HPET. When the counter is the HPET, its rate
                // says nothing about the processor's, and the line is left
                // out rather than wrong.
                if arch::vdso_can_read_counter() {
                    let khz = arch::counter_hz() / 1000;
                    put(
                        &mut out,
                        format_args!("cpu MHz\t\t: {}.{:03}\n", khz / 1000, khz % 1000),
                    );
                }
                put(
                    &mut out,
                    format_args!(
                        "apicid\t\t: {id}\ninitial apicid\t: {id}\n",
                        id = cpu.hardware_id
                    ),
                );
            }
            Arch::AArch64 => out.extend_from_slice(b"CPU architecture: 8\n"),
            Arch::Armv7a => out.extend_from_slice(b"CPU architecture: 7\n"),
        }
        out.push(b'\n');
    }
    Ok(out)
}

/// `/proc/filesystems`: the filesystem types `mount` takes, in the order
/// Linux registers them, sysfs first; only btrfs needs a block device.
///
/// The same names `syscall::fsctl`'s `filesystem_named` matches: a type this
/// file lists and `mount` refuses, or the other way round, is a program
/// deciding what to mount on a list the kernel does not keep.
pub(super) fn filesystems(_: &Kernel) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (name, nodev) in [
        (&b"sysfs"[..], true),
        (b"tmpfs", true),
        (b"proc", true),
        (b"cgroup2", true),
        (b"devtmpfs", true),
        (b"btrfs", false),
    ] {
        filesystems::render(&mut out, &Filesystem { name, nodev });
    }
    Ok(out)
}

/// `/proc/loadavg`: the averages (`super::loadavg`), the runnable tasks,
/// the live processes -- Linux counts threads there, and a process is what
/// this kernel keeps a count of -- and the last task number made.
pub(super) fn loadavg(_: &Kernel) -> Result<Vec<u8>> {
    let (running, loads) = super::loadavg::now();
    let total = crate::object::process::live()
        .map(|live| live.len() as u64)
        .map_err(|_| Errno::ENOMEM)?;
    let mut out = Vec::new();
    loadavg_text::render(
        &mut out,
        &Loadavg {
            loads,
            running,
            total,
            last: sched::tasks_made(),
        },
    );
    Ok(out)
}

/// `/proc/meminfo`.
///
/// Free memory is honestly all that is available, because nothing the kernel
/// holds can be reclaimed on demand: there is no page cache to shrink and no
/// swap. tmpfs file pages, which Linux reports under `Cached` and `Shmem`,
/// are held by VMOs here and are simply not free, so `Cached` is zero rather
/// than a figure that could be given back. `Slab` is the kernel heap's pages,
/// which is the same thing by another allocator.
pub(super) fn meminfo(_: &Kernel) -> Result<Vec<u8>> {
    let kib = |frames: u64| frames.saturating_mul(PAGE_SIZE / 1024);
    let free = kib(mm::free_frames());
    let info = Meminfo {
        total: kib(mm::managed_frames()),
        free,
        available: free,
        buffers: 0,
        cached: 0,
        swap_cached: 0,
        swap_total: 0,
        swap_free: 0,
        slab: kib(mm::heap_pages() as u64),
    };
    let mut out = Vec::new();
    meminfo::render(&mut out, &info);
    Ok(out)
}

/// `/proc/mounts`, with each mount point as the reader's root sees it.
///
/// The source is the filesystem's own name, which is what Linux shows for a
/// filesystem with no device. The options are the mount's own flags, each of
/// which the kernel enforces (`ferrix_vfs::MountFlags`), and `ro` for a
/// filesystem that takes no writes whatever the mount says.
pub(super) fn mounts(_: &Kernel) -> Result<Vec<u8>> {
    match process::current() {
        Some(caller) => process_mounts(&caller),
        None => {
            let ns = fs::namespace();
            mounts_from(ns, &ns.root())
        }
    }
}

/// `/proc/<pid>/ns/mnt`'s text, `mnt:[N]`: the number of the mount namespace
/// the process is in, which two processes share exactly when they are in
/// the same one, as Linux's nsfs inode number is compared.
pub(super) fn mount_namespace(process: &Process) -> Result<Vec<u8>> {
    let id = fs::namespace_of(&process.fs_context().lock()).id();
    Ok(alloc::format!("mnt:[{id}]").into_bytes())
}

/// `/proc/<pid>/ns/net`'s text, `net:[N]`.
pub(super) fn net_namespace(process: &Process) -> Result<Vec<u8>> {
    Ok(alloc::format!("net:[{}]", process.net_ns().id()).into_bytes())
}

/// `/proc/<pid>/ns/user`.
pub(super) fn user_namespace(process: &Process) -> Result<Vec<u8>> {
    let id = process.with_credentials(|held| held.user_ns.id());
    Ok(alloc::format!("user:[{id}]").into_bytes())
}

/// The user namespace the reading process is in: the first, for the kernel's
/// own reads.
fn reader_namespace() -> Arc<UserNamespace> {
    userns::acting().map_or_else(
        || Arc::clone(userns::first()),
        |reader| reader.with_credentials(|held| Arc::clone(&held.user_ns)),
    )
}

/// `/proc/<pid>/uid_map`, `gid_map` and `setgroups`, as the reader's
/// namespace shows them.
pub(super) fn id_map(process: &Process, file: super::MapFile) -> Vec<u8> {
    let namespace = process.with_credentials(|held| Arc::clone(&held.user_ns));
    match file {
        super::MapFile::Uid => userns::render_map(&namespace, IdKind::User, &reader_namespace()),
        super::MapFile::Gid => userns::render_map(&namespace, IdKind::Group, &reader_namespace()),
        super::MapFile::Setgroups => userns::render_setgroups(&namespace),
    }
}

/// A write to one of them: the namespace is the target process's, who opened
/// the file is `opener` and who writes is the running process, and
/// [`userns`] judges both.
pub(super) fn write_id_map(
    process: &Process,
    file: super::MapFile,
    opener: Option<&crate::syscall::credentials::Credentials>,
    data: &[u8],
) -> Result<usize> {
    let opener = opener.ok_or(Errno::EPERM)?;
    let writer = userns::acting()
        .ok_or(Errno::EPERM)?
        .with_credentials(|held| held.clone());
    let namespace = process.with_credentials(|held| Arc::clone(&held.user_ns));
    match file {
        super::MapFile::Uid => userns::write_map(&namespace, IdKind::User, opener, &writer, data),
        super::MapFile::Gid => userns::write_map(&namespace, IdKind::Group, opener, &writer, data),
        super::MapFile::Setgroups => userns::write_setgroups(&namespace, opener, &writer, data),
    }
}

/// The mount namespace `process` is in, and its root.
fn namespace_and_root(process: &Process) -> (Arc<Namespace>, Location) {
    let context = process.fs_context().lock();
    (fs::namespace_of(&context), context.root.clone())
}

/// `/proc/<pid>/mounts`: [`mounts`], with each mount point as that process's
/// root sees it rather than the reader's, as Linux has it. Programs read
/// `/proc/self/mounts` more often than `/proc/mounts`, which on Linux is a
/// link to it; btop reads nothing else when there is no `/etc/mtab`.
pub(super) fn process_mounts(process: &Process) -> Result<Vec<u8>> {
    let (ns, root) = namespace_and_root(process);
    mounts_from(&ns, &root)
}

/// The mount table, with each mount point as `root` sees it, and without
/// the mounts `root` cannot reach -- the namespace's bottom mount under `/`
/// among them -- as Linux's `show_vfsmnt` skips what `seq_path_root` cannot
/// name.
fn mounts_from(ns: &Namespace, root: &Location) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for mount in ns.mounts() {
        let at = Location {
            dentry: Arc::clone(mount.root()),
            mount: Arc::clone(&mount),
        };
        let Some(point) = ns.path_within(&at, root) else {
            continue;
        };
        let name = mount.filesystem().name().as_bytes();
        let options = shown_flags(&mount).options();
        mounts::render(
            &mut out,
            &Mount {
                source: name,
                point: &point,
                fstype: name,
                options: &options,
            },
        );
    }
    Ok(out)
}

/// A mount's flags as `/proc/mounts` shows them: its own, and read-only too
/// when its filesystem takes no writes, as Linux's `show_vfsmnt` prints
/// `ro` for either.
fn shown_flags(mount: &ferrix_vfs::Mount) -> MountFlags {
    if mount.filesystem_read_only() {
        mount.flags().union(MountFlags::READ_ONLY)
    } else {
        mount.flags()
    }
}

/// `/proc/<pid>/mountinfo`: every mount the process's root can reach, in the
/// order they were made -- so a mount's parent comes before it -- each with
/// its id, its parent's, its root inside its filesystem and its point from
/// that root. A mount the root cannot reach is left out, as Linux's
/// `seq_path_root` leaves it out; the namespace's root mount, above a root
/// that `/` was switched to, is one.
pub(super) fn mountinfo(process: &Process) -> Result<Vec<u8>> {
    let (ns, root) = namespace_and_root(process);
    let mut all = ns.mounts();
    all.sort_unstable_by_key(|mount| mount.id());
    let mut out = Vec::new();
    for mount in &all {
        let at = Location {
            dentry: Arc::clone(mount.root()),
            mount: Arc::clone(mount),
        };
        let Some(point) = ns.path_within(&at, &root) else {
            continue;
        };
        let parent = mount.parent().map_or(mount.id(), |(above, _)| above.id());
        let inside = Namespace::root_path(mount);
        let options = mount.flags().options();
        let name = mount.filesystem().name().as_bytes();
        let super_options: &[u8] = if mount.filesystem_read_only() {
            b"ro"
        } else {
            b"rw"
        };
        mounts::render_info(
            &mut out,
            &MountInfo {
                id: mount.id(),
                parent,
                device: mount.filesystem().device(),
                root: &inside,
                point: &point,
                options: &options,
                fstype: name,
                source: name,
                super_options,
            },
        );
    }
    Ok(out)
}

/// `/proc/uptime`: seconds on the counter, and the idle time summed over
/// processors, which is what the `idle` field of the `cpuN` lines in
/// `/proc/stat` adds up to.
pub(super) fn uptime(_: &Kernel) -> Result<Vec<u8>> {
    let nanos = time::now_nanos();
    let idle = online_times()?
        .iter()
        .fold(0_u64, |sum, (_, time)| sum.saturating_add(time.idle_ns));
    let mut out = Vec::new();
    put(
        &mut out,
        format_args!(
            "{}.{:02} {}.{:02}\n",
            nanos / NANOS,
            (nanos % NANOS) / (NANOS / 100),
            idle / NANOS,
            (idle % NANOS) / (NANOS / 100)
        ),
    );
    Ok(out)
}

/// Each online processor's time, by logical number.
///
/// With no topology there is one processor, the boot processor, which is
/// what [`online_cpus`] says too.
fn online_times() -> Result<Vec<(u32, sched::CpuTime)>> {
    let topology = smp::topology();
    let times = sched::cpu_times().map_err(|_| Errno::ENOMEM)?;
    Ok(times
        .into_iter()
        .enumerate()
        .filter(|(logical, _)| {
            topology.map_or(*logical == 0, |topology| {
                topology
                    .cpus()
                    .get(*logical)
                    .is_some_and(smp::PerCpu::is_online)
            })
        })
        .map(|(logical, time)| (u32::try_from(logical).unwrap_or(u32::MAX), time))
        .collect())
}

/// `/proc/stat`.
///
/// # Where each number comes from
///
/// * **The processor lines.** A run queue counts the nanoseconds it charged
///   while a task was running and while its idle task was, and the time since
///   its last charge goes to whichever it is doing now, so a line never goes
///   backwards between two reads — `top` and `mpstat` divide by the
///   difference. The kernel has no split between a task's user and kernel
///   time: a task is charged when the scheduler looks, not on each crossing.
///   So all busy time is reported as `user`, the side most of it is on for a
///   program, and `system` is zero; calling it all `system` would be the
///   same guess the other way, and splitting it by some ratio would be a
///   number with no source. `nice` is zero because nothing sets a nice value
///   yet, `iowait` because nothing waits on a block device, `irq` and
///   `softirq` because interrupt time is charged to what it interrupted, and
///   `steal` and the guest times because this kernel runs no guests. The
///   `cpu` line sums nanoseconds before converting, as Linux does, so it can
///   be a tick or so more than its `cpuN` lines added up.
/// * **`intr`**: every interrupt the controller handed to [`irq::dispatch`],
///   claimed or not, timer and inter-processor interrupts included. No count
///   is kept per number, so the list after the total is empty.
/// * **`ctxt`**: switches every run queue has made, idle task included.
/// * **`btime`**: the wall clock less the counter, in whole seconds. That is
///   zero until something sets the clock, and moves when it is set, as
///   Linux's does.
/// * **`processes`**: tasks made since boot.
/// * **`procs_running`**: tasks on the run queues, each running one included
///   and the idle tasks not.
/// * **`procs_blocked`**: zero. Linux counts tasks waiting on block I/O here,
///   and nothing does.
/// * **`softirq`**: zeros, as there are no softirqs.
pub(super) fn kstat(_: &Kernel) -> Result<Vec<u8>> {
    let tick = NANOS / CLOCK_TICKS;
    let times = online_times()?;
    let (mut busy, mut idle) = (0_u64, 0_u64);
    let mut cpus = Vec::with_capacity(times.len());
    for (logical, time) in &times {
        busy = busy.saturating_add(time.busy_ns);
        idle = idle.saturating_add(time.idle_ns);
        cpus.push((
            *logical,
            cpu_times(time.busy_ns / tick, time.idle_ns / tick),
        ));
    }
    // Every queue, not only the online processors': a queue whose processor
    // never joined has made no switch and holds no task.
    let all = sched::cpu_times().map_err(|_| Errno::ENOMEM)?;
    let switches = all
        .iter()
        .fold(0_u64, |sum, time| sum.saturating_add(time.switches));
    let running = all
        .iter()
        .fold(0_u64, |sum, time| sum.saturating_add(time.runnable as u64));
    let stat = Kstat {
        total: cpu_times(busy / tick, idle / tick),
        cpus: &cpus,
        interrupts: irq::delivered().saturating_add(irq::unclaimed()),
        per_interrupt: &[],
        context_switches: switches,
        boot_time: u64::try_from(time::realtime_offset()).unwrap_or(0) / NANOS,
        processes: sched::tasks_made(),
        running,
        blocked: 0,
        softirqs: 0,
        per_softirq: [0; kstat::SOFTIRQS],
    };
    let mut out = Vec::new();
    kstat::render(&mut out, &stat);
    Ok(out)
}

/// A processor line: busy ticks as `user`, idle as `idle`; see [`kstat`].
fn cpu_times(user: u64, idle: u64) -> CpuTimes {
    CpuTimes {
        user,
        idle,
        ..CpuTimes::default()
    }
}

/// `/proc/cmdline`: the command line the kernel was started with, and a
/// newline, as Linux writes it. What a program reads its own `ferrix.*`
/// settings from, the stat service's among them.
pub(super) fn cmdline_file(_: &Kernel) -> Result<Vec<u8>> {
    let line = super::command_line();
    let mut out = Vec::new();
    out.try_reserve(line.len() + 1).map_err(|_| Errno::ENOMEM)?;
    out.extend_from_slice(line);
    out.push(b'\n');
    Ok(out)
}

/// `/proc/version`: the release and version `uname` reports, in the sentence
/// Linux writes them in, with the compiler it names being the one that built
/// this kernel.
/// `/proc/ferrix-seam`: what crosses the seam (`fs::seam`). Not Linux's.
pub(super) fn seam(_: &Kernel) -> Result<Vec<u8>> {
    Ok(fs::seam::render())
}

pub(super) fn version(_: &Kernel) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    put(
        &mut out,
        format_args!("Linux version {RELEASE} (ferrix@ferrix) (rustc) {VERSION}\n"),
    );
    Ok(out)
}

/// `/proc/partitions`: a row per disk registered in `/dev`, in registration
/// order, its size in 1 KiB blocks as Linux counts them — sectors times the
/// sector size over 1024 — and empty, header too, with none. Partitions of a
/// disk are not rows yet: nothing here reads a partition table.
pub(super) fn partitions(_: &Kernel) -> Result<Vec<u8>> {
    let mut disks: Vec<(Vec<u8>, u32, u32, u64)> = Vec::new();
    devfs::for_each_block(|name, major, minor, device| {
        disks.push((
            name.to_vec(),
            major,
            minor,
            block::size_in_bytes(device) / 1024,
        ));
    });
    let rows: Vec<Partition<'_>> = disks
        .iter()
        .map(|(name, major, minor, blocks)| Partition {
            major: *major,
            minor: *minor,
            blocks: *blocks,
            name,
        })
        .collect();
    let mut out = Vec::new();
    partitions::render(&mut out, &rows);
    Ok(out)
}

// -- /proc/sys ------------------------------------------------------------------

/// A string value.
fn string(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    sysctl::string(&mut out, value);
    out
}

/// A number value.
fn number(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    sysctl::number(&mut out, value);
    out
}

/// `kernel/ostype`: `uname -s`.
pub(super) fn ostype(_: &Kernel) -> Result<Vec<u8>> {
    Ok(string(SYSNAME.as_bytes()))
}

/// `kernel/osrelease`: `uname -r`.
pub(super) fn osrelease(_: &Kernel) -> Result<Vec<u8>> {
    Ok(string(RELEASE.as_bytes()))
}

/// `kernel/version`: `uname -v`.
pub(super) fn sys_version(_: &Kernel) -> Result<Vec<u8>> {
    Ok(string(VERSION.as_bytes()))
}

/// `kernel/hostname`: `uname -n`.
pub(super) fn hostname(_: &Kernel) -> Result<Vec<u8>> {
    Ok(string(&nsproxy::acting().uts.hostname()))
}

/// `kernel/domainname`: the domain name `uname` reports.
pub(super) fn domainname(_: &Kernel) -> Result<Vec<u8>> {
    Ok(string(&nsproxy::acting().uts.domainname()))
}

/// A write to `kernel/hostname`: `proc_dostring`'s, so `echo name >` stores
/// `name`, and a name past `__NEW_UTS_LEN` is cut there rather than refused,
/// which is where the sysctl and `sethostname` differ on Linux too. The
/// write is taken as from the start of the value, whatever the offset.
pub(super) fn set_hostname(_: &Kernel, data: &[u8]) -> Result<usize> {
    nsproxy::acting()
        .uts
        .set_hostname(sysctl::stored(data, NAME_MAX))?;
    Ok(data.len())
}

/// A write to `kernel/domainname`, as [`set_hostname`].
pub(super) fn set_domainname(_: &Kernel, data: &[u8]) -> Result<usize> {
    nsproxy::acting()
        .uts
        .set_domainname(sysctl::stored(data, NAME_MAX))?;
    Ok(data.len())
}

/// `/proc/<pid>/oom_score_adj`: see `Process::oom_score_adj`.
pub(super) fn oom_score_adj(process: &Process) -> Result<Vec<u8>> {
    let mut out = alloc::format!("{}", process.oom_score_adj()).into_bytes();
    out.push(b'\n');
    Ok(out)
}

/// A write to `/proc/<pid>/oom_score_adj`: a decimal from -1000 to 1000,
/// with the newline `echo` gives it, or `EINVAL`, as `oom_score_adj_write`
/// answers.
pub(super) fn set_oom_score_adj(process: &Process, data: &[u8]) -> Result<usize> {
    let value = core::str::from_utf8(data)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .filter(|value| (-1000..=1000).contains(value))
        .ok_or(Errno::EINVAL)?;
    process.set_oom_score_adj(value);
    Ok(data.len())
}

/// The id a namespace shows for one it has no mapping of: Linux's
/// `DEFAULT_OVERFLOWUID` and `DEFAULT_OVERFLOWGID`, nobody and nogroup.
const OVERFLOW_ID: u64 = 65534;

/// `kernel/overflowuid`: the uid shown for one that cannot be shown, which
/// bubblewrap reads before it makes a sandbox and cannot run without
/// (`docs/NAMESPACES.md` §1.2). Read-only here: nothing in the kernel shows
/// another.
pub(super) fn overflowuid(_: &Kernel) -> Result<Vec<u8>> {
    Ok(number(OVERFLOW_ID))
}

/// `kernel/overflowgid`: [`overflowuid`]'s group.
pub(super) fn overflowgid(_: &Kernel) -> Result<Vec<u8>> {
    Ok(number(OVERFLOW_ID))
}

/// `kernel/pid_max`: one past the highest pid the registry hands out.
pub(super) fn pid_max(_: &Kernel) -> Result<Vec<u8>> {
    Ok(number(u64::from(PID_MAX)))
}

/// `vm/overcommit_memory`: 1, Linux's `OVERCOMMIT_ALWAYS`.
///
/// No mapping is refused for want of memory here: an object is a promise of
/// pages that the first touch of each pays for (`Vmo::committed`), and a
/// touch that finds no frame fails then, never the `mmap`. That is mode 1,
/// not Linux's default heuristic 0, which refuses a request plainly too
/// large up front.
///
/// Programs read it to choose how to give memory back. jemalloc, which
/// `rustc` links, does at start: on a system it believes does not
/// overcommit it decommits by mapping `PROT_NONE` over what it frees and
/// maps it back when it needs it, a `MAP_FIXED` pair each time, and with no
/// file to read that is what it believes.
pub(super) fn overcommit_memory(_: &Kernel) -> Result<Vec<u8>> {
    Ok(number(OVERCOMMIT_ALWAYS))
}

/// Linux's `OVERCOMMIT_ALWAYS`, from `include/uapi/linux/mman.h`.
pub(super) const OVERCOMMIT_ALWAYS: u64 = 1;

/// `fs/nr_open`: the most `RLIMIT_NOFILE` may be raised to.
pub(super) fn nr_open(_: &Kernel) -> Result<Vec<u8>> {
    Ok(number(u64::from(MAX_LIMIT)))
}

/// `fs/file-max`: `LONG_MAX`, which is no limit.
///
/// Linux's limit on files open across the system, which it sizes from memory
/// at boot and checks when a file is opened. Nothing here counts files open
/// across the system, so nothing limits them, and the value that says so is
/// the largest the sysctl accepts: `LONG_MAX` of the kernel's word, the number
/// systemd writes into it to mean unlimited.
pub(super) fn file_max(_: &Kernel) -> Result<Vec<u8>> {
    Ok(number(isize::MAX.unsigned_abs() as u64))
}

/// `/proc/sys/fs/mount-max`: the most mounts one namespace holds, its root
/// included, past which a mount, a bind or a copy is `ENOSPC`
/// (`ferrix_vfs::MOUNT_MAX`, Linux's default). Read-only here: a limit to
/// print, not one to raise.
pub(super) fn mount_max(_: &Kernel) -> Result<Vec<u8>> {
    Ok(number(ferrix_vfs::MOUNT_MAX as u64))
}

// -- A process ----------------------------------------------------------------

/// `/proc/<pid>/cmdline`: each argument followed by a NUL.
pub(super) fn cmdline(process: &Process) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for arg in process.args() {
        out.extend_from_slice(&arg);
        out.push(0);
    }
    Ok(out)
}

/// `/proc/<pid>/comm`.
/// `/proc/<pid>/cgroup`: the one line of the unified hierarchy, `0::/path`,
/// naming the cgroup -- the job -- the process is in, as the reader's cgroup
/// namespace sees it: relative to that namespace's root, and climbing out of
/// it with `/..` for a cgroup that is not beneath it (`docs/NAMESPACES.md`
/// §12).
pub(super) fn cgroup(process: &Process) -> Result<Vec<u8>> {
    let reader = nsproxy::acting().cgroup;
    fs::cgroupfs::proc_cgroup(&process.job(), reader.root())
}

/// `/proc/<pid>/ns/uts`'s text, `uts:[N]`.
pub(super) fn uts_namespace(process: &Process) -> Result<Vec<u8>> {
    Ok(alloc::format!("uts:[{}]", process.nsproxy().uts.id()).into_bytes())
}

/// `/proc/<pid>/ns/ipc`'s text, `ipc:[N]`.
pub(super) fn ipc_namespace(process: &Process) -> Result<Vec<u8>> {
    Ok(alloc::format!("ipc:[{}]", process.nsproxy().ipc.id()).into_bytes())
}

/// `/proc/<pid>/ns/cgroup`'s text, `cgroup:[N]`.
pub(super) fn cgroup_namespace(process: &Process) -> Result<Vec<u8>> {
    Ok(alloc::format!("cgroup:[{}]", process.nsproxy().cgroup.id()).into_bytes())
}

/// `/proc/<pid>/comm`.
pub(super) fn comm(process: &Process) -> Result<Vec<u8>> {
    let mut out = process.comm();
    out.push(b'\n');
    Ok(out)
}

/// `/proc/<pid>/task/<tid>/comm`: the process's name, since a thread has no
/// name of its own until `PR_SET_NAME` keeps one per thread.
pub(super) fn thread_comm(of: &ThreadOf) -> Result<Vec<u8>> {
    comm(&of.process)
}

/// `/proc/<pid>/exe`: the path of the file it was started from, from its own
/// root, with ` (deleted)` after a file since removed, as Linux reads it; the
/// path it was recorded as for a program from no file; or `ENOENT` for a
/// process nothing was started in.
pub(super) fn exe(process: &Process) -> Result<Vec<u8>> {
    if let Some(at) = process.exe_location() {
        let root = process.fs_context().lock().root.clone();
        return Ok(located(&at, &root));
    }
    let exe = process.exe();
    if exe.is_empty() {
        return Err(Errno::ENOENT);
    }
    Ok(exe)
}

/// `/proc/<pid>/fd/<fd>`: the path the descriptor was opened at, from the
/// process's own root, with ` (deleted)` after a name that has since been
/// removed — which is how a program recovers an unlinked temporary file.
pub(super) fn descriptor(process: &Process, fd: i32) -> Result<Vec<u8>> {
    let file = process
        .files()
        .lock()
        .get(fd)
        .map(Arc::clone)
        .map_err(|_| Errno::ENOENT)?;
    let root = process.fs_context().lock().root.clone();
    Ok(located(file.location(), &root))
}

/// `/proc/<pid>/cwd`: the working directory, as `getcwd` would give it —
/// with ` (deleted)` after a directory since removed, where `getcwd` answers
/// `ENOENT` instead, as Linux's link and system call differ too.
pub(super) fn cwd(process: &Process) -> Result<Vec<u8>> {
    let (cwd, root) = {
        let context = process.fs_context().lock();
        (context.cwd.clone(), context.root.clone())
    };
    Ok(located(&cwd, &root))
}

/// `/proc/<pid>/root`: the process's root, from itself, which is `/`.
pub(super) fn root(process: &Process) -> Result<Vec<u8>> {
    let root = process.fs_context().lock().root.clone();
    Ok(located(&root, &root))
}

/// Where `at` is, from `root`, marked ` (deleted)` if its name is gone.
///
/// The locations are clones taken under the process's lock and used with it
/// released: the path walks parent dentries, and the last reference to a
/// location may release a chain of them.
fn located(at: &Location, root: &Location) -> Vec<u8> {
    let mut path = fs::namespace().path_of(at, root);
    if at.dentry.is_unhashed() {
        path.extend_from_slice(b" (deleted)");
    }
    path
}

/// `/proc/<pid>/maps`.
///
/// A file mapping is named by its file's path, with the offset, device and
/// inode Linux prints for one, and ` (deleted)` after a file since removed.
/// That includes the program's own image and its linker's, whose pages the
/// loader maps from their files. What the loader copied instead -- the
/// partial pages at a segment's ends, `.bss`, an image from no file -- is
/// anonymous, as is everything else, since a file's offset, device and inode
/// there would describe a mapping that does not exist. So a segment shows as
/// its file's pages with an anonymous page or two either side, where Linux,
/// which maps the partial pages from the file too, shows one run. The
/// anonymous names are the ones the process knows — `[heap]` for the regions
/// `brk` made, `[stack]` for the one holding the stack pointer it was started
/// with, and `[vdso]` and `[vvar]` for the vDSO's image and the data page
/// below it.
pub(super) fn maps(process: &Process) -> Result<Vec<u8>> {
    let heap = process.heap_range();
    let stack = start_stack(process);
    let vdso = process.startup().map_or(0, |startup| startup.vdso);
    let vvar = vdso.saturating_sub(PAGE_SIZE);
    let root = process.fs_context().lock().root.clone();
    let mut out = Vec::new();
    let regions = process.space().regions().map_err(|_| Errno::ENOMEM)?;
    for region in regions {
        let file = region.file.and_then(|(id, offset)| {
            let file = process
                .space()
                .mapped_file(id)?
                .downcast::<OpenFile>()
                .ok()?;
            Some((file, offset))
        });
        let path;
        let (offset, dev, inode, name): (u64, u64, u64, Option<&[u8]>) = match &file {
            Some((file, offset)) => {
                path = located(file.location(), &root);
                let stat = fs::namespace().stat(file.location()).ok();
                (
                    *offset,
                    stat.map_or(0, |stat| stat.dev),
                    stat.map_or(0, |stat| stat.metadata.ino),
                    Some(path.as_slice()),
                )
            }
            None if heap.is_some_and(|(start, end)| region.start < end && start < region.end) => {
                (0, 0, 0, Some(b"[heap]"))
            }
            None if holds(&region, stack) => (0, 0, 0, Some(b"[stack]")),
            None if vdso != 0 && region.start == vdso => (0, 0, 0, Some(b"[vdso]")),
            None if vdso != 0 && region.start == vvar => (0, 0, 0, Some(b"[vvar]")),
            None => (0, 0, 0, None),
        };
        let mapping = Mapping {
            start: region.start,
            end: region.end,
            read: region.flags.read,
            write: region.flags.write,
            execute: region.flags.execute,
            shared: region.flags.shared,
            offset,
            major: crate::syscall::stat::major(dev),
            minor: crate::syscall::stat::minor(dev),
            inode,
            name,
        };
        maps::render(&mut out, &mapping, Width::native());
    }
    Ok(out)
}

/// The stack pointer the process's program was started with, or zero: what
/// names `[stack]`.
fn start_stack(process: &Process) -> u64 {
    process.startup().map_or(0, |startup| startup.stack)
}

/// Whether a region holds an address, zero never being one.
fn holds(region: &Region, address: u64) -> bool {
    address != 0 && region.start <= address && address < region.end
}

/// What `status` and `stat` say about a process's memory, in bytes.
struct Memory {
    /// Everything mapped.
    size: u64,
    /// The `mlock`ed part.
    locked: u64,
    /// Private writable memory that is not the stack.
    data: u64,
    /// The stack.
    stack: u64,
}

impl Memory {
    fn of(process: &Process) -> Memory {
        let stack_pointer = start_stack(process);
        let mut memory = Memory {
            size: 0,
            locked: 0,
            data: 0,
            stack: 0,
        };
        process.space().with_regions(|regions| {
            for region in regions {
                memory.add(&region, stack_pointer);
            }
        });
        memory
    }

    /// Count `region` in.
    fn add(&mut self, region: &Region, stack_pointer: u64) {
        let bytes = region.end.saturating_sub(region.start);
        self.size = self.size.saturating_add(bytes);
        if region.flags.locked {
            self.locked = self.locked.saturating_add(bytes);
        }
        if holds(region, stack_pointer) {
            self.stack = self.stack.saturating_add(bytes);
        } else if region.flags.write && !region.flags.shared {
            self.data = self.data.saturating_add(bytes);
        }
    }
}

/// What the thread numbered `tid` of `process` is doing, as Linux 7.0's
/// `stat` and `status` report it on the same programs:
///
/// * `Z` once the process has ended and is waiting to be reaped, whether it
///   exited, was killed, or was killed while stopped; and for a leader that
///   has ended while other threads of its process run.
/// * `T` while it is stopped by a signal, whatever its threads were doing.
/// * `D` while the thread waits in `vfork` for its child to let go.
/// * `R` for a thread that is running or waiting its turn, and `S` for one
///   waiting for something.
///
/// `t` is a tracer's stop, and nothing traces a process here.
fn state_of(process: &Process, tid: u32) -> State {
    if process.is_released() {
        return State::Zombie;
    }
    if process.is_stopped() {
        return State::Stopped;
    }
    // The reader is running, whether or not its process lists it: one the
    // kernel runs for a check lists neither its thread nor its task.
    let reader = crate::syscall::thread::current_of(process);
    if reader.is_some_and(|reader| reader.tid() == tid) {
        return State::Running;
    }
    let threads = process.threads();
    let Some(thread) = threads.iter().find(|thread| thread.tid() == tid) else {
        return if threads.is_empty() {
            State::Sleeping
        } else {
            State::Zombie
        };
    };
    if thread.in_vfork() {
        return State::DiskSleep;
    }
    let task = process
        .tasks()
        .into_iter()
        .find(|task| crate::syscall::thread::of_task(task).is_some_and(|of| of.tid() == tid));
    match task {
        Some(task) if !task.is_blocked() => State::Running,
        _ => State::Sleeping,
    }
}

/// Its parent's pid, as `getppid` answers it: 0 for a process the kernel
/// started, pid 1 among them, as Linux reports init's. This said 1 for every
/// process long after processes had parents, so a zombie in `/proc` always
/// looked like init's to reap, whoever its parent was.
fn parent_of(process: &Process) -> u32 {
    process.parent_pid()
}

/// Processors online, which every process may run on.
fn online_cpus() -> u32 {
    smp::topology().map_or(1, |topology| {
        u32::try_from(topology.online()).unwrap_or(u32::MAX)
    })
}

/// Its threads that have not begun to end, and never fewer than one: a
/// process the kernel made for a check runs a task without listing a thread,
/// and every process Linux shows has at least its main thread.
pub(super) fn thread_count(process: &Process) -> u32 {
    u32::try_from(process.threads().len())
        .unwrap_or(u32::MAX)
        .max(1)
}

/// `/proc/<pid>/status`.
///
/// `Umask` is the mask the process keeps and `umask` changes. `Uid` and `Gid`
/// are its real, effective, saved and filesystem ids, in that order.
pub(super) fn status(process: &Process) -> Result<Vec<u8>> {
    status_of(process, process.pid())
}

/// `/proc/<pid>/task/<tid>/status`: the process's, with the thread's own id
/// in `Pid`.
pub(super) fn thread_status(of: &ThreadOf) -> Result<Vec<u8>> {
    status_of(&of.process, of.tid)
}

/// A status file, for the thread numbered `tid` of `process`.
fn status_of(process: &Process, tid: u32) -> Result<Vec<u8>> {
    let reader = reader_namespace();
    let shown = |kind: IdKind, id: u32| userns::from_kid_munged(&reader, kind, id);
    let (uid, gid, caps) = process.with_credentials(|ids| {
        (
            [
                shown(IdKind::User, ids.user.real),
                shown(IdKind::User, ids.user.effective),
                shown(IdKind::User, ids.user.saved),
                shown(IdKind::User, ids.user.filesystem),
            ],
            [
                shown(IdKind::Group, ids.group.real),
                shown(IdKind::Group, ids.group.effective),
                shown(IdKind::Group, ids.group.saved),
                shown(IdKind::Group, ids.group.filesystem),
            ],
            capability_lines(ids),
        )
    });
    let memory = Memory::of(process);
    let name = process.comm();
    // Slots in the table as Linux sizes one: a power of two, 64 at least.
    let highest = process.files().lock().iter().map(|(fd, _)| fd).last();
    let fd_size = highest
        .and_then(|fd| u32::try_from(fd).ok())
        .map_or(64, |fd| fd.saturating_add(1).next_power_of_two().max(64));
    let status = Status {
        name: &name,
        umask: process.umask(),
        state: state_of(process, tid),
        tgid: process.pid(),
        pid: tid,
        ppid: parent_of(process),
        uid,
        gid,
        fd_size,
        vm_size: memory.size / 1024,
        vm_locked: memory.locked / 1024,
        vm_data: memory.data / 1024,
        vm_stack: memory.stack / 1024,
        threads: thread_count(process),
        cpus: online_cpus(),
    };
    let mut out = Vec::new();
    status::render(&mut out, &status);
    out.extend_from_slice(caps.as_bytes());
    Ok(out)
}

/// The `Cap*` lines of `status`. In the first namespace an effective uid of 0
/// stands for every capability and the sets are not enforced, so they read as
/// `capget` reports them; in a child they are the real ones.
fn capability_lines(
    credentials: &crate::syscall::credentials::Credentials,
) -> alloc::string::String {
    let [effective, permitted, inheritable] = if !credentials.user_ns.is_first() {
        [
            credentials.caps.effective,
            credentials.caps.permitted,
            credentials.caps.inheritable,
        ]
    } else if credentials.privileged() {
        [userns::FULL, userns::FULL, 0]
    } else {
        [0, 0, 0]
    };
    alloc::format!(
        "CapInh:\t{inheritable:016x}\nCapPrm:\t{permitted:016x}\nCapEff:\t{effective:016x}\n\
         CapBnd:\t{:016x}\nCapAmb:\t{:016x}\n",
        credentials.caps.bounding,
        0_u64
    )
}

/// `/proc/<pid>/stat`.
pub(super) fn stat(process: &Process) -> Result<Vec<u8>> {
    stat_of(process, process.pid())
}

/// `/proc/<pid>/task/<tid>/stat`: the process's, with the thread's own id
/// first, as Linux's per-thread `stat` starts.
pub(super) fn thread_stat(of: &ThreadOf) -> Result<Vec<u8>> {
    stat_of(&of.process, of.tid)
}

/// The processor time a `stat` file reports, in clock ticks: the whole
/// process's for `/proc/<pid>/stat`, where `tid` is the process's own id,
/// and the one thread's for `/proc/<pid>/task/<tid>/stat`, as Linux's are.
/// Until it was counted here, `top` and `ps` said every process was idle.
fn cpu_ticks(process: &Process, tid: u32) -> u64 {
    let nanos = if tid == process.pid() {
        time::process_runtime(process)
    } else {
        process
            .tasks()
            .iter()
            .filter(|task| {
                crate::syscall::thread::of_task(task).is_some_and(|thread| thread.tid() == tid)
            })
            .map(|task| task.runtime())
            .fold(0, u64::saturating_add)
    };
    nanos / (NANOS / CLOCK_TICKS)
}

/// The console's device, 5:1, as `stat`'s `tty_nr` encodes a device:
/// `new_encode_dev`, the minor's low byte, then the major.
const CONSOLE_TTY_NR: u32 = 5 << 8 | 1;

/// `stat`'s `tty_nr` and `tpgid` for a process in `session`: the console
/// and its foreground group when the console is that session's controlling
/// terminal, and Linux's "none", 0 and -1, otherwise.
///
/// A pseudo-terminal's session has one too, but nothing maps a session to
/// its pty, so such a process reads as having none, as every process did
/// before `setsid` was answered.
fn controlling_terminal(session: u32) -> (u32, i32) {
    let (holder, foreground) = fs::terminal::with(|t| (t.session, t.foreground));
    if session == 0 || holder != session {
        return (0, -1);
    }
    let tpgid = match foreground {
        0 => -1,
        group => i32::try_from(group).unwrap_or(-1),
    };
    (CONSOLE_TTY_NR, tpgid)
}

/// A stat file, for the thread numbered `tid` of `process`.
fn stat_of(process: &Process, tid: u32) -> Result<Vec<u8>> {
    let memory = Memory::of(process);
    let comm = process.comm();
    let session = process.sid();
    let (tty_nr, tpgid) = controlling_terminal(session);
    let stat = Stat {
        pid: tid,
        comm: &comm,
        state: state_of(process, tid),
        ppid: parent_of(process),
        pgrp: process.pgid(),
        session,
        tty_nr,
        tpgid,
        // No `PF_*` flag applies: in particular `PF_RANDOMIZE` is clear,
        // because nothing randomizes the layout.
        flags: 0,
        // The time its threads have run, all of it as user time: the
        // scheduler charges a task for its whole time on a processor and
        // does not split what it spent in the kernel.
        utime: cpu_ticks(process, tid),
        stime: 0,
        priority: 20,
        nice: 0,
        threads: thread_count(process),
        start_time: process.started() / (NANOS / CLOCK_TICKS),
        vsize: memory.size,
        // With no memory to count them in, none: `stat` is still answered.
        rss: process.space().resident_pages().unwrap_or(0),
        // `RLIM_INFINITY`, as an `unsigned long`.
        rss_limit: usize::MAX as u64,
        // A copying loader keeps no record of where the text was put.
        start_code: 0,
        end_code: 0,
        start_stack: start_stack(process),
        // The signal state belongs to `syscall::signal`, which has no way to
        // read it from outside a handler yet; zero is "none", which is true
        // of pending signals and may not be of the rest.
        pending: 0,
        blocked: 0,
        ignored: 0,
        caught: 0,
        exit_signal: SIGCHLD,
        processor: smp::this_cpu().map_or(0, |cpu| u32::try_from(cpu.logical).unwrap_or(0)),
        start_brk: process.heap_range().map_or(0, |(start, _)| start),
        // The startup image's addresses are `src/lib/kernel/ustack`'s, and not kept.
        arg_start: 0,
        arg_end: 0,
        env_start: 0,
        env_end: 0,
    };
    let mut out = Vec::new();
    stat::render(&mut out, &stat);
    Ok(out)
}

// ---------------------------------------------------------------------------
// /proc/net
//
// Every one of these asks the net core for what it knows and hands it to
// `src/lib/fs/procfs`, which is where the formats are pinned. Nothing here decides a
// column.
// ---------------------------------------------------------------------------

/// `/proc/net/dev`.
pub(super) fn net_dev(_: &Kernel) -> Result<Vec<u8>> {
    let devices: Vec<(Vec<u8>, procfs_net::DeviceCounters)> =
        crate::net::acting().core().with(|stack, _| {
            stack
                .interfaces()
                .iter()
                .map(|interface| {
                    let counters = interface.counters;
                    (
                        interface.name.as_bytes().to_vec(),
                        procfs_net::DeviceCounters {
                            received_bytes: counters.received_bytes,
                            received: counters.received,
                            received_errors: counters.received_errors,
                            received_dropped: counters.received_dropped,
                            multicast: 0,
                            sent_bytes: counters.sent_bytes,
                            sent: counters.sent,
                            sent_errors: counters.sent_errors,
                            sent_dropped: counters.sent_dropped,
                        },
                    )
                })
                .collect()
        });
    let rows: Vec<procfs_net::Device<'_>> = devices
        .iter()
        .map(|(name, counters)| procfs_net::Device {
            name,
            counters: *counters,
        })
        .collect();
    let mut out = Vec::new();
    procfs_net::dev(&mut out, &rows);
    Ok(out)
}

/// `/proc/net/route`, which is IPv4 only, as it is on Linux.
pub(super) fn net_route(_: &Kernel) -> Result<Vec<u8>> {
    let rows: Vec<(Vec<u8>, procfs_net::Route<'static>)> =
        crate::net::acting().core().with(|stack, _| {
            stack
                .routes()
                .entries()
                .iter()
                .filter_map(|route| {
                    let IpAddress::V4(destination) = route.destination.address() else {
                        return None;
                    };
                    let interface = stack.interface(route.interface)?;
                    let gateway = match route.gateway {
                        Some(IpAddress::V4(address)) => address.octets(),
                        _ => [0; 4],
                    };
                    // RTF_UP is 1 and RTF_GATEWAY 2, which is what `route` prints
                    // as `U` and `UG`.
                    let flags = 1 | u16::from(route.gateway.is_some()) << 1;
                    Some((
                        interface.name.as_bytes().to_vec(),
                        procfs_net::Route {
                            interface: b"",
                            destination: destination.octets(),
                            gateway,
                            flags,
                            metric: route.metric,
                            mask: mask_of(route.destination.prefix_len()),
                            mtu: interface.mtu,
                        },
                    ))
                })
                .collect()
        });
    let rows: Vec<procfs_net::Route<'_>> = rows
        .iter()
        .map(|(name, route)| procfs_net::Route {
            interface: name,
            ..*route
        })
        .collect();
    let mut out = Vec::new();
    procfs_net::route(&mut out, &rows);
    Ok(out)
}

/// The netmask a prefix length names, in network order.
fn mask_of(prefix_len: u8) -> [u8; 4] {
    let bits = u32::from(prefix_len).min(32);
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    mask.to_be_bytes()
}

/// `/proc/net/tcp`.
pub(super) fn net_tcp(_: &Kernel) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    procfs_net::tcp(&mut out, &inet_sockets(true, false));
    Ok(out)
}

/// `/proc/net/tcp6`.
pub(super) fn net_tcp6(_: &Kernel) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    procfs_net::tcp(&mut out, &inet_sockets(true, true));
    Ok(out)
}

/// `/proc/net/udp`.
pub(super) fn net_udp(_: &Kernel) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    procfs_net::udp(&mut out, &inet_sockets(false, false));
    Ok(out)
}

/// `/proc/net/udp6`.
pub(super) fn net_udp6(_: &Kernel) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    procfs_net::udp(&mut out, &inet_sockets(false, true));
    Ok(out)
}

/// The sockets of one protocol and one family, as rows.
///
/// A row names the socket's file, as `/proc/<pid>/fd` does, and the user that
/// owns it; both are looked up after the net core is let go.
fn inet_sockets(stream: bool, six: bool) -> Vec<procfs_net::Socket> {
    let reader = crate::net::acting();
    let mut rows: Vec<(ferrix_net::SocketId, procfs_net::Socket)> =
        reader.core().with(|stack, _| {
            stack
                .sockets()
                .filter(|(_, socket)| {
                    let is_stream = matches!(
                        socket,
                        ferrix_net::Socket::Stream(_) | ferrix_net::Socket::Listen(_)
                    );
                    let is_six = matches!(socket.family(), ferrix_net::Family::V6);
                    // A raw socket is neither TCP's nor UDP's: Linux lists it in
                    // `/proc/net/raw`, which this file tree does not have yet.
                    let is_raw = matches!(socket, ferrix_net::Socket::Raw(_));
                    !is_raw && is_stream == stream && is_six == six
                })
                .enumerate()
                .map(|(slot, (id, socket))| (id, socket_row(slot, socket)))
                .collect()
        });
    for (id, row) in &mut rows {
        if let Some(file) = crate::net::socket::file_of(reader.id(), *id) {
            row.inode = file.ino;
            row.uid = file.uid;
        }
    }
    rows.into_iter().map(|(_, row)| row).collect()
}

/// One socket as a row.
fn socket_row(slot: usize, socket: &ferrix_net::Socket) -> procfs_net::Socket {
    let remote = socket.remote().unwrap_or(ferrix_net::Endpoint::new(
        socket.local().address.unspecified(),
        0,
    ));
    let (state, transmit, receive) = match socket {
        ferrix_net::Socket::Stream(stream) => (
            stream.connection.state().procfs_code(),
            stream.connection.send_queued() as u32,
            stream.connection.receive_queued() as u32,
        ),
        // Linux reports a listening TCP socket as `TCP_LISTEN` and an
        // unconnected UDP one as `TCP_CLOSE`, which is 7 -- the same number a
        // closed stream has.
        ferrix_net::Socket::Listen(listener) if listener.backlog > 0 => (10, 0, 0),
        ferrix_net::Socket::Listen(_) => (7, 0, 0),
        ferrix_net::Socket::Udp(datagram) | ferrix_net::Socket::Icmp(datagram) => {
            (7, 0, datagram.queued() as u32)
        }
        ferrix_net::Socket::Raw(raw) => (7, 0, raw.datagram.queued() as u32),
    };
    procfs_net::Socket {
        slot,
        local: endpoint_row(socket.local()),
        remote: endpoint_row(remote),
        state,
        transmit_queue: transmit,
        receive_queue: receive,
        uid: 0,
        inode: 0,
    }
}

/// An endpoint as the file spells it.
fn endpoint_row(endpoint: ferrix_net::Endpoint) -> procfs_net::Endpoint {
    match endpoint.address {
        IpAddress::V4(address) => procfs_net::Endpoint::V4(address.octets(), endpoint.port),
        IpAddress::V6(address) => procfs_net::Endpoint::V6(address.octets(), endpoint.port),
    }
}

/// `/proc/net/arp`.
pub(super) fn net_arp(_: &Kernel) -> Result<Vec<u8>> {
    let rows: Vec<(Vec<u8>, procfs_net::Neighbour<'static>)> =
        crate::net::acting().core().with(|stack, _| {
            stack
                .neighbors()
                .entries()
                .iter()
                .filter_map(|entry| {
                    let IpAddress::V4(address) = entry.address else {
                        return None;
                    };
                    let mac = entry.mac?;
                    let interface = stack.interface(entry.interface)?;
                    Some((
                        interface.name.as_bytes().to_vec(),
                        procfs_net::Neighbour {
                            address: address.octets(),
                            flags: u32::from(entry.state.nud()),
                            hardware: mac,
                            interface: b"",
                        },
                    ))
                })
                .collect()
        });
    let rows: Vec<procfs_net::Neighbour<'_>> = rows
        .iter()
        .map(|(name, entry)| procfs_net::Neighbour {
            interface: name,
            ..*entry
        })
        .collect();
    let mut out = Vec::new();
    procfs_net::arp(&mut out, &rows);
    Ok(out)
}
