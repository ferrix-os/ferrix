//! How the Linux personality starts programs for the certified item: pid 1
//! from the root filesystem, lent to `init` as an
//! [`init::Launcher`](crate::init::Launcher), and a native process from an
//! image, lent to the native ABI and `devmgr` as
//! [`native::Processes`](crate::syscall::native::Processes).
//!
//! `init` is part of the certified item and decides which program runs;
//! opening that program in `/`, following its `#!` line, reading the dynamic
//! linker it names and starting it as a Linux process are all above the item
//! (`docs/certification/ITEM.md`), so they are here, and `main.rs` registers
//! them with [`install`] at bring-up. A native process is the same: the item
//! decides who may make one and in which job, and the ELF loader and the
//! process it loads into are this personality's.
//!
//! And `process_give`, the native call by which a Linux program hands its
//! child a bootstrap handle before the child's `execve` (`docs/INIT.md` §6,
//! K3): whose child a process is, and whether it has completed an `execve`,
//! are this personality's to say, and the move itself is the item's
//! ([`native::give_bootstrap`]).

use alloc::format;
use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::status;

use crate::fs;
use crate::hooks::Full;
use crate::init::{Failure, Image, Launcher, Opened, Start};
use crate::object::process::{self as core_process, Host};
use crate::syscall::credentials::Credentials;
use crate::syscall::exec::{self, ExecError};
use crate::syscall::load::{LoadError, Source};
use crate::syscall::native::{self, Argument, Processes, StartRefused};
use crate::syscall::process::{self, Process};
use crate::syscall::{attributes, registry};
use crate::user::space::SpaceError;

/// What `init` is lent.
static LAUNCHER: Launcher = Launcher {
    open,
    interpreter_line: exec::interpreter_line,
    start,
};

/// What the native ABI and `devmgr` are lent.
static PROCESSES: Processes = Processes {
    load: load_native,
    start: start_native,
    must_leave,
};

/// Lend `init` this personality's way of starting a program, and the native
/// ABI its way of making and starting a process, and answer `process_give`.
///
/// Called once from `main.rs`, before the boot marker; `init::run` is the
/// only caller of the first and runs after it, and nothing makes a native
/// process before `devmgr` is started.
///
/// # Errors
///
/// [`Full`] when the item has no room for `process_give`'s handler.
pub(crate) fn install() -> Result<(), Full> {
    crate::init::register_launcher(&LAUNCHER);
    native::register_processes(&PROCESSES);
    native::serve(NativeCall::ProcessGive, process_give)
}

/// `process_give(pid, handle)`: move `handle` into the bootstrap slot of the
/// caller's child `pid`, which must not have completed an `execve`.
///
/// The pid must be a process's own, not one of its threads' numbers, which
/// the table also finds it by. Its parent is compared by identity, so a pid
/// reused by an unrelated process is refused as not the caller's child. The
/// `execve` question is the slot's own: `execve` seals it
/// ([`crate::object::process::Process::seal_bootstrap`]) under the same lock
/// the move is judged under.
fn process_give(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [pid, handle, ..] = *registers;
    let parent = process::of_host(caller).ok_or(status::NO_PROCESS)?;
    let pid = u32::try_from(pid).map_err(|_| status::NO_PROCESS)?;
    let child = registry::find(pid)
        .filter(|child| child.pid() == pid)
        .ok_or(status::NO_PROCESS)?;
    let is_child = child
        .parent()
        .is_some_and(|its| core::ptr::eq(Arc::as_ptr(&its), parent));
    if !is_child {
        return Err(status::NOT_CHILD);
    }
    native::give_bootstrap(parent.core(), child.core(), Handle::from_register(handle))
}

/// A native process with `image` loaded, named `name`, made and not started,
/// running as `creator` runs, or as root with none ([`native::LoadNative`]).
///
/// "As `creator` runs" is its ids and its fs context: its mount namespace,
/// its root and its working directory (`docs/NAMESPACES.md` §2.5). A child
/// started in the first namespace's root instead would be an escape from a
/// namespace, or a `chroot`, in one call. A process with no creator -- one
/// the kernel starts, as `devmgr` is -- starts in the first namespace's
/// root, as root.
///
/// A creator of another personality's has no ids to give, and is refused
/// rather than given root's.
///
/// It also takes its creator's `no_new_privs` and dumpability, as a fork
/// child takes its parent's ([`attributes::inherit`]). A native process can
/// make Linux calls -- a number outside the native range goes to the Linux
/// table -- so without the flag a child of a process that gave up new
/// privileges could `execve` a set-user-id file and take them back. One with
/// no creator starts from the defaults.
pub(crate) fn load_native(
    creator: Option<&dyn Host>,
    image: &[u8],
    name: &[u8],
) -> Result<Arc<dyn Host>, Errno> {
    let creator = creator
        .map(|creator| process::of_host(creator).ok_or(status::BAD_STATE))
        .transpose()?;
    let (credentials, context) = match creator {
        Some(creator) => {
            let context = creator.fs_context().lock().clone();
            (creator.with_credentials(|held| held.clone()), Some(context))
        }
        None => (Credentials::root(), None),
    };
    let child = exec::load_native(image, name, credentials, context).map_err(load_status)?;
    // After `exec::load_native` registered it, so that the entry is not
    // pruned as a pid nothing finds, and before anything can start it.
    if let Some(creator) = creator {
        attributes::inherit(creator, &child);
        // And its seccomp mode and filter chain, for the thread that starts it:
        // a native process can make Linux calls, so a child that did not keep
        // its creator's filter would be one call out of the sandbox.
        crate::syscall::seccomp::inherit_native(creator, &child);
    }
    let process: Arc<dyn Host> = child;
    Ok(process)
}

/// The status a failed native load travels as: a fault in the image is the
/// caller's mistake, running out of memory is not.
fn load_status(error: ExecError) -> Errno {
    match error {
        ExecError::Load(LoadError::Space(SpaceError::OutOfMemory) | LoadError::Copy(_))
        | ExecError::Space(_)
        | ExecError::Startup
        | ExecError::Start(_) => status::NO_MEMORY,
        // A native process is loaded from bytes and never from a path, so it
        // brings no linker and this cannot arise; it is the caller's image
        // that would be at fault if it did.
        ExecError::Load(_) | ExecError::Linker(_) => status::INVALID_ARGS,
    }
}

/// Start `host`, which [`load_native`] made: claim it, make its first task,
/// then take the argument and run it.
///
/// In that order so that a race is harmless and a failure clean, which
/// `process_start` relies on: a second start is refused by the claim before
/// anything moved, and everything that can fail about the task has failed
/// before `argument` moves a handle into the process. A refused argument
/// drops the prepared start, which frees the task and gives the start back.
fn start_native(host: &Arc<dyn Host>, argument: Argument<'_>) -> Result<(), StartRefused> {
    let process =
        core_process::downcast::<Process>(Arc::clone(host)).ok_or(StartRefused::Claimed)?;
    let claim = process::claim_start(&process).map_err(|_| StartRefused::Claimed)?;
    let prepared = claim.prepare(None).map_err(|_| StartRefused::NoTask)?;
    let argument = argument().map_err(StartRefused::Argument)?;
    let _task = prepared.start(argument);
    Ok(())
}

/// Whether the calling thread of `caller` is to leave a native wait: its
/// process is ending, or another of its threads is replacing the program.
fn must_leave(caller: &dyn Host) -> bool {
    match process::of_host(caller) {
        Some(process) => process.caller_must_leave(),
        None => caller.core().is_terminated(),
    }
}

/// Open the program at `path` in the context a new process starts in: the
/// btrfs root once it is in place, the initramfs before that.
fn open(path: &[u8]) -> Result<Opened, Errno> {
    let context = fs::root_disk::process_context();
    let (program, exe, _set_ids) = fs::open_program(&context, None, path)?;
    Ok(Opened { program, exe })
}

/// Start `start` as pid 1, with the dynamic linker it names read from the
/// same root and its bootstrap in its slot, and wait for it to end.
///
/// Set-id bits are not honoured for pid 1: it starts as root already, as
/// the built-in program always has.
fn start(start: Start<'_>) -> Result<i32, Failure> {
    let image = match start.image {
        Image::BuiltIn(bytes) => Source::Bytes(bytes),
        Image::File(program) => Source::File(program),
    };
    run(start, image).map_err(|problem| match problem {
        ExecError::Linker(errno) => Failure::Linker(errno),
        other => Failure::Exec(format!("{other:?}")),
    })
}

/// [`start`], in the personality's own terms. The bootstrap goes into the
/// process between its load and its start, and is closed if it never runs.
fn run(start: Start<'_>, image: Source<'_>) -> Result<i32, ExecError> {
    let context = fs::root_disk::process_context();
    let linker = match exec::linker_for(&context, image) {
        Ok(linker) => linker,
        Err(errno) => {
            crate::object::dispose(start.bootstrap.map(|(object, _)| object));
            return Err(ExecError::Linker(errno));
        }
    };
    let executable = exec::Executable {
        image,
        exe: start.exe,
        exec_fn: start.exec_fn,
        set_ids: fs::SetIds::NONE,
        interpreter: linker.as_ref().map(Source::File),
    };
    exec::run_init(
        executable,
        start.argv,
        start.env,
        exec::random_bytes(),
        start.bootstrap,
    )
}
