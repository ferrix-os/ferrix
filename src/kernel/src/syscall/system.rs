//! What the system calls itself, what it has, and how it stops.
//!
//! # Why `sysname` says `Ferrix`
//!
//! The system names itself. `uname -s` reads `Ferrix`, chosen by the owner of
//! the project on 2026-09-13 over the earlier answer, `Linux`.
//!
//! That earlier answer had a reason, and it is the cost of this one: a program
//! that asks is often deciding what to do rather than printing a label.
//! Configure scripts (`config.guess`), build systems and bootstrap scripts
//! (rustc's `bootstrap.py`) map `uname -s` to a target, and may refuse a name
//! they do not know or take a path nobody has tested. The system call
//! interface is still Linux's, and the rest of the identity still says so:
//! `release` is a Linux version and `machine` the Linux architecture name, so
//! a program that reads those, or `/proc/version`, sees a Linux kernel. When a
//! build that Ferrix has to run branches on the name, it is told which target
//! to use rather than left to guess -- the fix goes there, not back here.
//! `src/lib/proto/linux-abi` says the same thing at [`Utsname::sysname`].
//!
//! `uname -a` reads
//!
//! ```text
//! Ferrix ferrix 6.1.0-ferrix #1 Ferrix 0.1.0 x86_64 GNU/Linux
//! ```
//!
//! `nodename` and `domainname` are the two a program may change, with
//! `sethostname` and `setdomainname`, and `uname` reports whatever they were
//! last set to. They are system-wide, as they are on Linux outside a UTS
//! namespace.
//!
//! [`Utsname::sysname`]: ferrix_linux_abi::types::Utsname::sysname

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::{Arch, PAGE_SIZE, is_user_address};
use ferrix_kmem::Charge;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_sync::{IrqControl, Once};

use crate::sync::SpinLock;

use crate::arch;
use crate::console::{log, println};
use crate::mm;
use crate::smp;
use crate::syscall::attributes::int;
use crate::syscall::credentials;
use crate::syscall::nsproxy;
use crate::syscall::process::Process;
use crate::syscall::time;
use crate::syscall::uaccess::{self, WORD};
use crate::syscall::userns::{self, UserNamespace};

/// Bytes in each of `utsname`'s six fields.
const FIELD: usize = 65;

/// The longest name `sethostname` accepts: `__NEW_UTS_LEN`, a field less its
/// terminator.
pub(crate) const NAME_MAX: usize = FIELD - 1;

/// The system's name, `uname -s`; see the module documentation for why.
pub(crate) const SYSNAME: &str = "Ferrix";

/// The kernel release.
///
/// Not arbitrary: a configure script compares this against a minimum, and
/// glibc refuses to start under a kernel it reads as older than the one it was
/// built for. So it is a plausible modern Linux version with Ferrix named in
/// the suffix, which is exactly what a distribution kernel does.
pub(crate) const RELEASE: &str = "6.1.0-ferrix";

/// The version string, which by convention starts with a build number.
pub(crate) const VERSION: &str = "#1 Ferrix 0.1.0";

/// A name set by `sethostname` or `setdomainname`: its bytes, NUL-padded, and
/// how many of them were given.
type SetName = ([u8; NAME_MAX], usize);

/// The host name before anything sets one.
const HOSTNAME_DEFAULT: &str = "ferrix";

/// The domain name before anything sets one. `(none)` is what Linux reports
/// before anything sets it.
const DOMAINNAME_DEFAULT: &str = "(none)";

/// What a UTS namespace's names are: each `None` until something sets it,
/// which reads as its default.
#[derive(Debug, Clone, Copy)]
struct Names {
    /// The host name.
    node: Option<SetName>,
    /// The NIS domain name.
    domain: Option<SetName>,
}

/// A UTS namespace: a host name and a domain name (`docs/NAMESPACES.md` §12).
#[derive(Debug)]
pub(crate) struct UtsNamespace {
    /// What `/proc/<pid>/ns/uts` names.
    id: u64,
    /// The user namespace that was current when it was made: a holder of
    /// `CAP_SYS_ADMIN` over it may set the names.
    owner: Arc<UserNamespace>,
    /// The names. A leaf lock: nothing is done under it but a copy of bytes.
    names: SpinLock<Names>,
    /// The kernel heap this is, charged to the job that made it (F-37).
    _charge: Option<Charge>,
}

/// The first UTS namespace, made on first use.
pub(crate) fn initial_uts() -> &'static Arc<UtsNamespace> {
    static FIRST: Once<Arc<UtsNamespace>> = Once::new();
    FIRST.call_once(|| {
        Arc::new(UtsNamespace {
            id: nsproxy::UTS_INIT_ID,
            owner: Arc::clone(userns::first()),
            names: SpinLock::new(Names {
                node: None,
                domain: None,
            }),
            _charge: None,
        })
    })
}

/// `default` as a name: its bytes, NUL-padded, and how many there are.
fn default_name(default: &str) -> SetName {
    let mut bytes = [0_u8; NAME_MAX];
    for (slot, byte) in bytes.iter_mut().zip(default.bytes()) {
        *slot = byte;
    }
    (bytes, default.len())
}

/// A name as `uname` reports it, without the padding.
fn unpadded(name: SetName) -> Vec<u8> {
    name.0.get(..name.1).unwrap_or_default().to_vec()
}

/// A name of `bytes`, if it fits: at most [`NAME_MAX`] bytes, or `EINVAL`.
fn name_of(bytes: &[u8]) -> Result<SetName, Errno> {
    let mut padded = [0_u8; NAME_MAX];
    padded
        .get_mut(..bytes.len())
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(bytes);
    Ok((padded, bytes.len()))
}

impl UtsNamespace {
    /// What `/proc/<pid>/ns/uts` names.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// The user namespace that owns it.
    pub(crate) fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }

    /// The names as they are now, in one look.
    pub(crate) fn names_padded(&self) -> (SetName, SetName) {
        let names = *self.names.lock();
        (
            names.node.unwrap_or_else(|| default_name(HOSTNAME_DEFAULT)),
            names
                .domain
                .unwrap_or_else(|| default_name(DOMAINNAME_DEFAULT)),
        )
    }

    /// The host name `uname` reports as `nodename`.
    pub(crate) fn hostname(&self) -> Vec<u8> {
        unpadded(self.names_padded().0)
    }

    /// The domain name `uname` reports as `domainname`.
    pub(crate) fn domainname(&self) -> Vec<u8> {
        unpadded(self.names_padded().1)
    }

    /// Whether something has set the host name, rather than it reading as
    /// the default.
    pub(crate) fn hostname_is_set(&self) -> bool {
        self.names.lock().node.is_some()
    }

    /// Set the host name, as `sethostname` does once it has read the name
    /// and judged the caller.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a name past [`NAME_MAX`].
    pub(crate) fn set_hostname(&self, name: &[u8]) -> Result<(), Errno> {
        let name = name_of(name)?;
        self.names.lock().node = Some(name);
        Ok(())
    }

    /// Set the domain name, as [`UtsNamespace::set_hostname`].
    ///
    /// # Errors
    ///
    /// `EINVAL` for a name past [`NAME_MAX`].
    pub(crate) fn set_domainname(&self, name: &[u8]) -> Result<(), Errno> {
        let name = name_of(name)?;
        self.names.lock().domain = Some(name);
        Ok(())
    }

    /// A namespace that starts with this one's names and is owned by
    /// `owner`: Linux's `clone_uts_ns`.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory.
    pub(crate) fn copy(&self, owner: Arc<UserNamespace>) -> Result<Arc<UtsNamespace>, Errno> {
        let charge = Charge::arc::<UtsNamespace>().map_err(|_| Errno::ENOMEM)?;
        let names = *self.names.lock();
        crate::fallible::try_arc(UtsNamespace {
            id: nsproxy::next_id(),
            owner,
            names: SpinLock::new(names),
            _charge: Some(charge),
        })
        .map_err(|_| Errno::ENOMEM)
    }
}

/// Whether something has set the first namespace's host name.
pub(crate) fn hostname_is_set() -> bool {
    initial_uts().hostname_is_set()
}

/// Answer `call` if it is one of this module's.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
) -> Option<Result<usize, Errno>> {
    let answer = match call {
        Syscall::Sethostname => sys_sethostname(process, a[0], int(a[1])),
        Syscall::Setdomainname => sys_setdomainname(process, a[0], int(a[1])),
        Syscall::Getcpu => sys_getcpu(process, a[0], a[1]),
        Syscall::Syslog => sys_syslog(process, int(a[0]), a[1], int(a[2])),
        Syscall::Reboot => sys_reboot(process, a[0] as u32, a[1] as u32, a[2] as u32, a[3]),
        _ => return None,
    };
    Some(answer)
}

/// `uname`.
pub(crate) fn sys_uname(process: &Process, at: u64) -> Result<usize, Errno> {
    // The name Linux uses for the machine, which is not always the name this
    // tree uses for the architecture: 32-bit Arm is `armv7l` to `uname` and
    // `armv7a` here.
    let machine = match arch::ARCH {
        Arch::X86_64 => "x86_64",
        Arch::AArch64 => "aarch64",
        Arch::Armv7a => "armv7l",
    };
    let (node, domain) = process.nsproxy().uts.names_padded();
    let fields: [&[u8]; 6] = [
        SYSNAME.as_bytes(),
        node.0.get(..node.1).unwrap_or_default(),
        RELEASE.as_bytes(),
        VERSION.as_bytes(),
        machine.as_bytes(),
        domain.0.get(..domain.1).unwrap_or_default(),
    ];

    // Built whole and copied once. Every field is NUL-padded because the
    // structure is fixed-width and a reader stops at the first NUL.
    let mut buffer = [0_u8; FIELD * 6];
    for (slot, text) in buffer.chunks_mut(FIELD).zip(fields) {
        for (byte, source) in slot.iter_mut().take(NAME_MAX).zip(text) {
            *byte = *source;
        }
    }
    uaccess::copy_to_user(process.space(), at, &buffer).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// Read a name for `sethostname` or `setdomainname`.
///
/// `len` bytes exactly, NULs included, as `kernel/sys.c` copies them: the
/// length is the caller's statement of what the name is.
fn read_name(process: &Process, at: u64, len: i32) -> Result<SetName, Errno> {
    let len = usize::try_from(len)
        .ok()
        .filter(|&len| len <= NAME_MAX)
        .ok_or(Errno::EINVAL)?;
    let mut bytes = [0_u8; NAME_MAX];
    let name = bytes.get_mut(..len).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, name).map_err(|_| Errno::EFAULT)?;
    Ok((bytes, len))
}

/// The UTS namespace `process` is in, if it holds `CAP_SYS_ADMIN` over the
/// user namespace that owns it: Linux's `ns_capable(uts_ns->user_ns,
/// CAP_SYS_ADMIN)`. Root in the first namespace does; fake root does for the
/// namespace it made and never for the first's.
fn nameable(process: &Process) -> Result<Arc<UtsNamespace>, Errno> {
    let uts = process.nsproxy().uts;
    let allowed = process
        .with_credentials(|held| userns::capable_over(held, uts.owner(), userns::CAP_SYS_ADMIN));
    if allowed { Ok(uts) } else { Err(Errno::EPERM) }
}

/// `sethostname`: `CAP_SYS_ADMIN` over the namespace's owner, and at most 64
/// bytes, or `EINVAL`.
pub(crate) fn sys_sethostname(process: &Process, at: u64, len: i32) -> Result<usize, Errno> {
    let uts = nameable(process)?;
    let (bytes, len) = read_name(process, at, len)?;
    uts.set_hostname(bytes.get(..len).ok_or(Errno::EINVAL)?)?;
    Ok(0)
}

/// `setdomainname`: as `sethostname`, for the other field.
pub(crate) fn sys_setdomainname(process: &Process, at: u64, len: i32) -> Result<usize, Errno> {
    let uts = nameable(process)?;
    let (bytes, len) = read_name(process, at, len)?;
    uts.set_domainname(bytes.get(..len).ok_or(Errno::EINVAL)?)?;
    Ok(0)
}

/// Put the first namespace's host name back to what it was before the boot
/// checks changed it.
pub(crate) fn forget_hostname() {
    initial_uts().names.lock().node = None;
}

/// Bytes in `struct sysinfo` (`linux/sysinfo.h`): a `long` of uptime, three
/// load averages and six memory counts as `unsigned long`s, two `__u16`s,
/// two more counts, a `__u32` unit and `20 - 2 * word - 4` bytes of padding,
/// rounded to a word. 112 on the 64-bit pair and 64 on ARMv7-A; checked with
/// `sizeof` and `offsetof` against the header compiled for x86-64 and for
/// `arm-linux-gnueabihf` (AArch64 is LP64 with no override of it).
pub(crate) const SYSINFO_SIZE: usize = sysinfo_size(WORD);

/// [`SYSINFO_SIZE`] for a program whose `long` is `word` bytes.
const fn sysinfo_size(word: usize) -> usize {
    (word * 11 + 20).next_multiple_of(word)
}
const _: () = assert!(
    SYSINFO_SIZE == if WORD == 8 { 112 } else { 64 },
    "struct sysinfo is 112 bytes on LP64 and 64 on ILP32"
);

/// Where each field of `struct sysinfo` is, in native words, from the same
/// `offsetof` check: `procs` at 80 or 40, `mem_unit` at 104 or 52.
/// Fractional bits in `sysinfo`'s loads: Linux's `SI_LOAD_SHIFT`.
const SI_LOAD_SHIFT: u32 = 16;

pub(crate) mod sysinfo_at {
    use super::WORD;
    /// `uptime`.
    pub(crate) const UPTIME: usize = 0;
    /// `loads[3]`, three words.
    pub(crate) const LOADS: usize = WORD;
    /// `totalram`.
    pub(crate) const TOTALRAM: usize = WORD * 4;
    /// `freeram`.
    pub(crate) const FREERAM: usize = WORD * 5;
    /// `procs`, a `__u16`.
    pub(crate) const PROCS: usize = WORD * 10;
    /// `mem_unit`, a `__u32`.
    pub(crate) const MEM_UNIT: usize = WORD * 13;
}

/// `sysinfo`.
///
/// The memory counts are the frame allocator's, which is what `free` prints.
/// The load averages are `/proc/loadavg`'s, in sixteen fractional bits
/// (`SI_LOAD_SHIFT`) rather than its eleven, which is what `getloadavg`
/// divides by. Shared and buffer memory, swap and high memory are zero
/// because none of them exists here.
///
/// `mem_unit` is chosen as `kernel/sys.c`'s `do_sysinfo` chooses it: bytes
/// (unit 1) when the total fits in an `unsigned long`, pages otherwise. On a
/// 64-bit build that is always bytes. On ARMv7-A a machine with 4 GiB or more
/// reports pages, because a count of bytes would wrap in 32 bits and `free`
/// would print a small machine.
///
/// Uptime rounds up, as Linux's does, so a machine that has been up for any
/// time at all has been up for at least a second.
pub(crate) fn sys_sysinfo(process: &Process, at: u64) -> Result<usize, Errno> {
    sys_sysinfo_at_width(process, at, WORD)
}

/// [`sys_sysinfo`] into the layout of a program whose `long` is `word` bytes:
/// an i386 program's is ARMv7-A's 64 bytes, Linux's `compat_sysinfo`.
pub(crate) fn sys_sysinfo_at_width(
    process: &Process,
    at: u64,
    word: usize,
) -> Result<usize, Errno> {
    // Every field offset in `sysinfo_at` is a whole number of native words,
    // and the same number of words at any width: rescale it.
    let field = |native: usize| native / WORD * word;
    let nanos = time::now_nanos();
    let uptime = nanos / 1_000_000_000 + u64::from(!nanos.is_multiple_of(1_000_000_000));
    let total_pages = mm::managed_frames();
    let free_pages = mm::free_frames();
    let word_max = if word == 8 {
        u64::MAX
    } else {
        u64::from(u32::MAX)
    };
    let (unit, total, free) = match total_pages.checked_mul(PAGE_SIZE) {
        Some(bytes) if bytes <= word_max => (1_u32, bytes, free_pages * PAGE_SIZE),
        _ => (PAGE_SIZE as u32, total_pages, free_pages),
    };
    let live = crate::object::process::live().map_err(|_| Errno::ENOMEM)?;
    let procs = u16::try_from(live.len()).unwrap_or(u16::MAX);
    drop(live);

    let mut bytes = [0_u8; 112];
    let buffer = bytes.get_mut(..sysinfo_size(word)).ok_or(Errno::EINVAL)?;
    let mut put = |at: usize, value: &[u8]| {
        if let Some(slot) = buffer.get_mut(at..at + value.len()) {
            slot.copy_from_slice(value);
        }
    };
    let long = |value: u64| value.to_le_bytes();
    put(
        field(sysinfo_at::UPTIME),
        long(uptime).get(..word).unwrap_or_default(),
    );
    put(
        field(sysinfo_at::TOTALRAM),
        long(total).get(..word).unwrap_or_default(),
    );
    put(
        field(sysinfo_at::FREERAM),
        long(free).get(..word).unwrap_or_default(),
    );
    let (_, loads) = crate::fs::procfs::loadavg::now();
    for (index, load) in loads.into_iter().enumerate() {
        let shifted = load << (SI_LOAD_SHIFT - ferrix_procfs::loadavg::FSHIFT);
        put(
            field(sysinfo_at::LOADS) + index * word,
            long(shifted).get(..word).unwrap_or_default(),
        );
    }
    put(field(sysinfo_at::PROCS), &procs.to_le_bytes());
    put(field(sysinfo_at::MEM_UNIT), &unit.to_le_bytes());
    uaccess::copy_to_user(process.space(), at, buffer).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `getcpu`: the logical number of the processor this is running on, and
/// NUMA node 0, which is the only node.
///
/// The per-CPU record is read with interrupts masked, so that the register it
/// is found through and the record it names belong to the same processor --
/// a preemption between the two could otherwise migrate the task and report
/// a number that was never true. The answer can be stale by the time the
/// program reads it, as it can on Linux; that is the nature of the call. The
/// third argument, a cache, has been ignored since Linux 2.6.24.
pub(crate) fn sys_getcpu(process: &Process, cpu_at: u64, node_at: u64) -> Result<usize, Errno> {
    let saved = <arch::Irq as IrqControl>::disable();
    let cpu = smp::this_cpu().map_or(0, |record| record.logical);
    <arch::Irq as IrqControl>::restore(saved);
    if cpu_at != 0 {
        uaccess::put_u32(process.space(), cpu_at, u32::try_from(cpu).unwrap_or(0))?;
    }
    if node_at != 0 {
        uaccess::put_u32(process.space(), node_at, 0)?;
    }
    Ok(0)
}

/// How often a `SYSLOG_ACTION_READ` waiting for the log looks at it again.
///
/// The console never wakes a reader -- recording a byte is all it may do, from
/// any context (`console::log`) -- so a waiting reader looks for itself: often
/// enough that `dmesg -w` follows a boot as it prints, and seldom enough to
/// cost nothing while nothing does.
const READ_POLL_NANOS: u64 = 15_000_000;

/// The most a read copies out under the reader's lock at once, through the
/// stack.
const CHUNK: usize = 512;

/// `SYSLOG_ACTION_READ`'s cursor: one for the whole system, as Linux's
/// `syslog_seq` is, so that two readers share the log rather than each read it
/// all. A place in the kernel log's sequence (`console::log`).
static READER: SpinLock<u64> = SpinLock::new(0);

/// Where `SYSLOG_ACTION_CLEAR` left the log: `READ_ALL` reads nothing before
/// it. Linux's `clear_seq`.
static CLEARED: AtomicU64 = AtomicU64::new(0);

/// `syslog`, the kernel log's system call.
///
/// The log is the console's: every byte it sent since boot, the kernel's
/// lines and programs' output alike, as they were written, in a ring of
/// [`log::CAPACITY`] bytes (`console::log`). The actions are Linux's
/// (`kernel/printk/printk.c`), over raw bytes rather than records: a line
/// carries no `<level>` prefix, which busybox's `dmesg` prints as it is.
///
/// * `READ` (2) takes what the one system-wide reader has not read yet, and
///   waits for something if there is nothing, interruptibly and restarted
///   under `SA_RESTART` as a blocking read is. The console never wakes it, so
///   it looks again every [`READ_POLL_NANOS`].
/// * `READ_ALL` (3) copies the newest `len` bytes kept since the last clear,
///   and `READ_CLEAR` (4) does the same and then clears.
/// * `CLEAR` (5) makes `READ_ALL` start from here; the reader's place is its
///   own, as on Linux.
/// * `SIZE_UNREAD` (9) is what `READ` would have to read, and `SIZE_BUFFER`
///   (10) the ring's length.
/// * Close, open, console off and on, and the console level (0, 1, 6, 7, 8)
///   succeed and change nothing: the log has nothing to open, and the console
///   prints everything.
///
/// **Every action is privileged**, `READ_ALL` and `SIZE_BUFFER` included: this
/// is Linux with `dmesg_restrict` on, which is Linux's own hardened default.
/// The log holds every program's console output, which is that program's and
/// not the reader's, and a kernel log is where kernel addresses turn up. The
/// kernel keeps the lines that print its layout out of the log
/// (`console::write_unlogged`), but that is a list somebody maintains, and a
/// privilege check is the line that does not depend on it.
pub(crate) fn sys_syslog(
    process: &Process,
    action: i32,
    buf: u64,
    len: i32,
) -> Result<usize, Errno> {
    credentials::require_privilege(process)?;
    match action {
        // Close, open, console off, console on.
        0 | 1 | 6 | 7 => Ok(0),
        2..=4 => {
            let Some(len) = user_buffer(buf, len)? else {
                return Ok(0);
            };
            match action {
                2 => read_waiting(process, buf, len),
                _ => read_all(process, buf, len, action == 4),
            }
        }
        5 => {
            let _ = CLEARED.fetch_max(log::written(), Ordering::Relaxed);
            Ok(0)
        }
        // Console level.
        8 if (1..=8).contains(&len) => Ok(0),
        9 => Ok(clamp(log::unread(*READER.lock()))),
        10 => Ok(log::CAPACITY),
        _ => Err(Errno::EINVAL),
    }
}

/// A read's buffer, checked as Linux checks it: `None` for a length of zero,
/// which reads nothing.
fn user_buffer(buf: u64, len: i32) -> Result<Option<usize>, Errno> {
    if buf == 0 || len < 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(None);
    }
    let last = buf.checked_add(u64::from(len.unsigned_abs()) - 1);
    if !is_user_address(buf) || !last.is_some_and(is_user_address) {
        return Err(Errno::EFAULT);
    }
    Ok(usize::try_from(len.unsigned_abs()).ok())
}

/// A count of bytes, as a system call's answer.
fn clamp(count: u64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}

/// `SYSLOG_ACTION_READ`: what the reader has not read, up to `len` bytes and
/// at least one, waiting until there is one.
///
/// It waits on its process's signals, never on its process's end, which only
/// this thread leaving can bring about; a process ended while it waits counts
/// as a signal pending.
fn read_waiting(process: &Process, buf: u64, len: usize) -> Result<usize, Errno> {
    loop {
        let copied = read_unread(process, buf, len)?;
        if copied != 0 {
            return Ok(copied);
        }
        let deadline = crate::timer::now_nanos().saturating_add(READ_POLL_NANOS);
        let _ = process
            .signalled()
            .wait_until_deadline(|| process.signal_pending(), deadline);
        if process.signal_pending() {
            return Err(Errno::ERESTARTSYS);
        }
    }
}

/// Take up to `len` bytes from the reader's place, at most a [`CHUNK`], and
/// copy them to `buf`. Bytes the log dropped before the reader came for them
/// are skipped, as Linux skips records it no longer holds.
fn read_unread(process: &Process, buf: u64, len: usize) -> Result<usize, Errno> {
    let mut chunk = [0u8; CHUNK];
    let room = chunk.get_mut(..len.min(CHUNK)).unwrap_or_default();
    let copied = {
        let mut cursor = READER.lock();
        // A reader parked past the log's end (`park_reader`) stays there
        // rather than being brought back to it.
        if log::unread(*cursor) == 0 {
            return Ok(0);
        }
        log::read(&mut cursor, room).copied
    };
    let taken = room.get(..copied).unwrap_or_default();
    uaccess::copy_to_user(process.space(), buf, taken).map_err(|_| Errno::EFAULT)?;
    Ok(copied)
}

/// `SYSLOG_ACTION_READ_ALL`, and with `clear` `READ_CLEAR`: the newest `len`
/// bytes kept since the last clear, oldest first.
fn read_all(process: &Process, buf: u64, len: usize, clear: bool) -> Result<usize, Errno> {
    let end = log::written();
    let from = end.saturating_sub(len as u64);
    let mut cursor = from.max(CLEARED.load(Ordering::Relaxed));
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    while cursor < end && done < len {
        let wanted = clamp(end.saturating_sub(cursor)).min(len - done);
        let room = chunk.get_mut(..wanted.min(CHUNK)).unwrap_or_default();
        let read = log::read(&mut cursor, room);
        let taken = room.get(..read.copied).unwrap_or_default();
        let at = buf.saturating_add(done as u64);
        uaccess::copy_to_user(process.space(), at, taken).map_err(|_| Errno::EFAULT)?;
        done = done.saturating_add(read.copied);
    }
    if clear {
        let _ = CLEARED.fetch_max(end, Ordering::Relaxed);
    }
    Ok(done)
}

/// Park `SYSLOG_ACTION_READ`'s reader past the end of the log, so that a read
/// waits however much is logged, and answer where it was. For the syscall
/// check's program that must stay blocked in the read until it is killed;
/// [`unpark_reader`] puts it back.
pub(crate) fn park_reader() -> u64 {
    core::mem::replace(&mut *READER.lock(), u64::MAX)
}

/// Put `SYSLOG_ACTION_READ`'s reader back where [`park_reader`] found it.
pub(crate) fn unpark_reader(at: u64) {
    *READER.lock() = at;
}

/// `LINUX_REBOOT_MAGIC1` and the four `MAGIC2`s (`linux/reboot.h`): Linus's
/// and his daughters' birthdays, and the guard against a stray call.
const REBOOT_MAGIC1: u32 = 0xFEE1_DEAD;
/// See [`REBOOT_MAGIC1`].
const REBOOT_MAGIC2: [u32; 4] = [0x2812_1969, 0x0512_1996, 0x1604_1998, 0x2011_2000];

/// The `reboot` commands this kernel answers, from `linux/reboot.h`.
mod command {
    /// Restart the machine.
    pub(super) const RESTART: u32 = 0x0123_4567;
    /// Stop the machine without powering it off.
    pub(super) const HALT: u32 = 0xCDEF_0123;
    /// Let Ctrl-Alt-Del restart the machine.
    pub(super) const CAD_ON: u32 = 0x89AB_CDEF;
    /// Send Ctrl-Alt-Del to init instead.
    pub(super) const CAD_OFF: u32 = 0;
    /// Power the machine off.
    pub(super) const POWER_OFF: u32 = 0x4321_FEDC;
    /// Restart with a command string for the firmware.
    pub(super) const RESTART2: u32 = 0xA1B2_C3D4;
}

/// `reboot`: power off, halt or restart the machine, after checking the magic
/// numbers that keep a stray call from doing it.
///
/// Power-off and halt both call the architecture's `shutdown`, which is the
/// kernel's own way of stopping and powers the machine off where it can --
/// halting without powering off would leave a QEMU run waiting forever for a
/// machine that has nothing more to say. Restart calls the architecture's
/// `reset`, which powers off instead where firmware cannot reset: a machine
/// that was asked to come back and did not is noticed, and a kernel that
/// pretended to reset and carried on running is not. The Ctrl-Alt-Del
/// switches are accepted; there is no keyboard to send the combination.
///
/// Every command that stops the machine commits `/` and `/data` first, as the
/// kernel's own power-off does (`power::sync_disks`). Linux leaves that to the
/// caller, and a caller that skips it -- busybox's `poweroff -f -n`, or a
/// program that only knows the call -- loses the last transaction there; an
/// init that synced first costs this a second sync with nothing to commit
/// (`docs/INIT.md` §8.2, K7).
///
/// The line printed first is the one Linux prints, so a log reads the same.
/// Commands for kexec and suspend are `EINVAL`, as on a kernel built without
/// them.
pub(crate) fn sys_reboot(
    process: &Process,
    magic1: u32,
    magic2: u32,
    cmd: u32,
    arg: u64,
) -> Result<usize, Errno> {
    // `CAP_SYS_BOOT` before the magic numbers, as Linux checks.
    credentials::require_privilege(process)?;
    if magic1 != REBOOT_MAGIC1 || !REBOOT_MAGIC2.contains(&magic2) {
        return Err(Errno::EINVAL);
    }
    if matches!(
        cmd,
        command::POWER_OFF | command::HALT | command::RESTART | command::RESTART2
    ) {
        crate::power::sync_disks();
        let action = match cmd {
            command::POWER_OFF => ferrix_audit::power::OFF,
            command::HALT => ferrix_audit::power::HALT,
            _ => ferrix_audit::power::RESTART,
        };
        let caller = crate::object::process::Host::core(process);
        crate::audit::power(crate::audit::Subject::of(caller), action);
    }
    match cmd {
        command::CAD_ON | command::CAD_OFF => Ok(0),
        command::POWER_OFF => {
            println!("reboot: Power down");
            arch::shutdown()
        }
        command::HALT => {
            println!("reboot: System halted");
            arch::shutdown()
        }
        command::RESTART => {
            println!("reboot: Restarting system");
            arch::reset()
        }
        command::RESTART2 => {
            // Linux copies at most 255 bytes into a 256-byte buffer and cuts
            // a longer word off there; a word with no NUL in that reach is
            // EFAULT here instead, which no caller that means it will meet.
            let mut word = Vec::new();
            uaccess::copy_cstr_from_user(process.space(), arg, 255, &mut word)
                .map_err(|_| Errno::EFAULT)?;
            let word = core::str::from_utf8(&word).unwrap_or("");
            println!("reboot: Restarting system with command '{word}'");
            // Where the firmware reads a word, on the one machine that has
            // one: a DK board's U-Boot, whose support registers with power.
            match crate::power::request_boot_mode(word) {
                Ok(what) => println!("reboot: {what}"),
                Err(why) => println!("reboot: '{word}' changes nothing: {why}"),
            }
            arch::reset()
        }
        _ => Err(Errno::EINVAL),
    }
}
