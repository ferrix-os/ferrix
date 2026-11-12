//! What a thread is doing, read from outside it (`docs/DEBUG.md` §1):
//! `/proc/<pid>/task/<tid>/syscall`, `wchan` and `stack`, the first thread's
//! as `/proc/<pid>/syscall`, `wchan` and `stack`, and `/proc/<pid>/environ`.
//!
//! The view of the task is `sched::inspect`'s; this names what it found the
//! way Linux's files do. Who may open which is decided in `procfs.rs`
//! ([`super::guard_of`]) before any of these runs; `wchan` alone is open to
//! everyone, and says `0` to a reader who may not look into the process, as
//! Linux's `proc_pid_wchan` does.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;

use ferrix_native_abi::nr as native;
use ferrix_vfs::{Errno, Result};

use super::ThreadOf;
use crate::arch;
use crate::sched::Task;
use crate::sched::inspect::{self, Call, MAX_FRAMES};
use crate::syscall::process::Process;
use crate::syscall::{thread, uaccess};
use crate::trap::Abi;

/// The most of a process's environment `environ` reads: as much as `execve`
/// lets a program start with, which is all of it.
const ENVIRON_MOST: u64 = 2 * 1024 * 1024;

/// What one piece of `environ`'s read takes: a page, so that no read faults
/// in more than it copies.
const ENVIRON_PIECE: usize = 4096;

/// The system call names whose first argument is a descriptor, after which
/// `wchan` names what the descriptor is.
const ON_A_DESCRIPTOR: &[&str] = &[
    "read",
    "write",
    "readv",
    "writev",
    "pread64",
    "pwrite64",
    "preadv",
    "pwritev",
    "preadv2",
    "pwritev2",
    "ioctl",
    "recv",
    "recvfrom",
    "recvmsg",
    "recvmmsg",
    "send",
    "sendto",
    "sendmsg",
    "sendmmsg",
    "accept",
    "accept4",
    "connect",
    "epoll_wait",
    "epoll_pwait",
    "epoll_pwait2",
    "fsync",
    "fdatasync",
    "flock",
    "fcntl",
    "fcntl64",
    "getdents64",
    "splice",
    "tee",
];

/// The task that runs thread `tid` of `process`, if it has one.
fn task_of(process: &Process, tid: u32) -> Option<Arc<Task>> {
    process
        .tasks()
        .into_iter()
        .find(|task| thread::of_task(task).is_some_and(|of| of.tid() == tid))
}

/// Whether the reader is thread `tid` itself, which is running as it reads.
fn is_reader(process: &Process, tid: u32) -> bool {
    thread::current_of(process).is_some_and(|reader| reader.tid() == tid)
}

/// What the thread waits in, or `None` while it runs.
fn call(process: &Process, tid: u32) -> Option<Call> {
    if is_reader(process, tid) {
        return None;
    }
    let task = task_of(process, tid)?;
    inspect::call_of(&task, process.program_abi())
}

/// Linux's `/proc/<pid>/syscall` text for thread `tid`.
fn syscall_text(process: &Process, tid: u32) -> Vec<u8> {
    let mut out = alloc::string::String::new();
    let _ = match call(process, tid) {
        None => writeln!(out, "running"),
        Some(Call::Trapped { sp, pc }) => writeln!(out, "-1 {sp:#x} {pc:#x}"),
        Some(Call::Blocked {
            number,
            args,
            sp,
            pc,
        }) => {
            let [a, b, c, d, e, f] = args;
            writeln!(
                out,
                "{} {a:#x} {b:#x} {c:#x} {d:#x} {e:#x} {f:#x} {sp:#x} {pc:#x}",
                number as i64
            )
        }
    };
    out.into_bytes()
}

/// `CamelCase` as `snake_case`: a call's name in its table's enum as Linux
/// names it (`EpollPwait` is `epoll_pwait`, `Pread64` `pread64`).
fn snake(camel: &str) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    for (index, character) in camel.char_indices() {
        if character.is_ascii_uppercase() {
            if index != 0 {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// The name of call `number` in the table of the entry `abi` names: Ferrix's
/// own calls by their native names, Linux's by Linux's.
fn call_name(abi: Abi, number: u64) -> Option<alloc::string::String> {
    let number = usize::try_from(number).ok()?;
    if abi == Abi::Native && native::is_native(number) {
        return native::decode(number).map(|call| snake(&alloc::format!("{call:?}")));
    }
    let call = match abi {
        Abi::Native => arch::decode_syscall(number),
        Abi::Compat => arch::decode_compat_syscall(number),
    }?;
    Some(snake(&alloc::format!("{call:?}")))
}

/// Linux's `/proc/<pid>/wchan` text for thread `tid`: one token, no newline,
/// as Linux writes it.
fn wchan_text(process: &Process, tid: u32) -> Vec<u8> {
    if super::may_inspect(process.pid()).is_err() {
        return b"0".to_vec();
    }
    let text = match call(process, tid) {
        None => alloc::string::String::from("0"),
        Some(Call::Trapped { .. }) => alloc::string::String::from("fault"),
        Some(Call::Blocked { number, args, .. }) => {
            match call_name(process.program_abi(), number) {
                None => alloc::format!("syscall_{number}"),
                Some(name) if ON_A_DESCRIPTOR.contains(&name.as_str()) => {
                    // The descriptor as the argument names it now: one closed
                    // and reused since the call began names its successor,
                    // as Linux's would.
                    let described = i32::try_from(args[0] as i64)
                        .ok()
                        .and_then(|fd| super::render::descriptor(process, fd).ok());
                    match described {
                        Some(target) => {
                            alloc::format!(
                                "{name}:{}",
                                alloc::string::String::from_utf8_lossy(&target)
                            )
                        }
                        None => name,
                    }
                }
                Some(name) => name,
            }
        }
    };
    // One token: a descriptor's text with a space in it would be read as two
    // columns by `ps`.
    text.replace(' ', "_").into_bytes()
}

/// Linux's `/proc/<pid>/stack` text for thread `tid`: a line per frame, the
/// address where the image was linked, so that `cargo xtask symbolize`
/// names it from the ELF.
fn stack_text(process: &Process, tid: u32) -> Vec<u8> {
    let mut out = alloc::string::String::new();
    if is_reader(process, tid) {
        return out.into_bytes();
    }
    let Some(task) = task_of(process, tid) else {
        return out.into_bytes();
    };
    let mut chain = [0_u64; MAX_FRAMES];
    if let Some(found) = inspect::chain_of(&task, &mut chain) {
        for address in chain.iter().take(found) {
            let _ = writeln!(out, "[<0>] {address:#018x}");
        }
    }
    out.into_bytes()
}

/// `EOPNOTSUPP` where the view reads no call (`inspect::calls_are_visible`),
/// rather than a file that would say `running` of a thread that is not.
fn calls_visible() -> Result<()> {
    if inspect::calls_are_visible() {
        Ok(())
    } else {
        Err(Errno::EOPNOTSUPP)
    }
}

/// `/proc/<pid>/task/<tid>/syscall`.
pub(super) fn thread_syscall(of: &ThreadOf) -> Result<Vec<u8>> {
    calls_visible()?;
    Ok(syscall_text(&of.process, of.tid))
}

/// `/proc/<pid>/task/<tid>/wchan`.
pub(super) fn thread_wchan(of: &ThreadOf) -> Result<Vec<u8>> {
    Ok(wchan_text(&of.process, of.tid))
}

/// `/proc/<pid>/task/<tid>/stack`.
pub(super) fn thread_stack(of: &ThreadOf) -> Result<Vec<u8>> {
    Ok(stack_text(&of.process, of.tid))
}

/// `/proc/<pid>/syscall`: its first thread's.
pub(super) fn syscall(process: &Process) -> Result<Vec<u8>> {
    calls_visible()?;
    Ok(syscall_text(process, process.pid()))
}

/// `/proc/<pid>/wchan`: its first thread's.
pub(super) fn wchan(process: &Process) -> Result<Vec<u8>> {
    Ok(wchan_text(process, process.pid()))
}

/// `/proc/<pid>/stack`: its first thread's.
pub(super) fn stack(process: &Process) -> Result<Vec<u8>> {
    Ok(stack_text(process, process.pid()))
}

/// `/proc/<pid>/environ`: the bytes between the first environment string's
/// first byte and the last one's terminator, read from the process's own
/// memory as Linux reads `env_start..env_end` (p2-139). A page at a time,
/// faulting each in the process's space as its own fault would be; a page
/// it cannot read ends the file there, as Linux's `access_remote_vm` stops
/// short. No procfs lock is held.
pub(super) fn environ(process: &Process) -> Result<Vec<u8>> {
    let (start, end) = process.env_range();
    let len = end.saturating_sub(start).min(ENVIRON_MOST);
    let mut out = Vec::new();
    out.try_reserve(usize::try_from(len).map_err(|_| Errno::ENOMEM)?)
        .map_err(|_| Errno::ENOMEM)?;
    let space = Arc::clone(process.space());
    let mut piece = [0_u8; ENVIRON_PIECE];
    let mut at = start;
    while at < start.saturating_add(len) {
        let want = usize::try_from(start.saturating_add(len) - at)
            .unwrap_or(ENVIRON_PIECE)
            .min(ENVIRON_PIECE);
        let Some(target) = piece.get_mut(..want) else {
            break;
        };
        if uaccess::copy_from_user(&space, at, target).is_err() {
            break;
        }
        out.extend_from_slice(target);
        at = at.saturating_add(want as u64);
    }
    Ok(out)
}
