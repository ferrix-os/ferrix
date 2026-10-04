//! Turning an ELF image into a running program.
//!
//! The three pieces that already exist — the loader, `src/lib/kernel/ustack`, and the
//! architecture's way into ring 3 — meet here, and they meet in exactly one
//! place on purpose. The first process and `execve` need the same two numbers
//! by different routes, and the way those two routes drift apart is by each
//! assembling the numbers itself.
//!
//! # Where the stack comes from, and why not from the loader
//!
//! The loader maps what the ELF says to map and nothing else. A stack is not
//! in the ELF: its size is a policy, its address is a policy, and what goes on
//! it — the argument vector, the environment, the auxiliary vector — comes
//! from the caller and from the loader's *results*, not from the image. So the
//! loader returns `AT_PHDR` and friends and this function decides the rest.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    AT_BASE, AT_CLKTCK, AT_EGID, AT_EMPTY_PATH, AT_ENTRY, AT_EUID, AT_FDCWD, AT_GID, AT_HWCAP,
    AT_HWCAP2, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM, AT_SECURE, AT_SYMLINK_NOFOLLOW,
    AT_SYSINFO_EHDR, AT_UID,
};
use ferrix_ustack::{Spec, Width};
use ferrix_vfs::access::MAY_EXEC;
use ferrix_vfs::{Access, Context, FileType, OpenFile, OpenFlags};

use crate::fs::SetIds;
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::fallible::AllocError;
use crate::object::{self, Transfer};
use crate::syscall::credentials::Credentials;
use crate::syscall::fd;
use crate::syscall::load::{self, LoadError, Source};
use crate::syscall::process::{self, Process, Startup};
use crate::syscall::program::ProgramFile;
use crate::syscall::registry;
use crate::syscall::uaccess;
use crate::trap::Abi;
use crate::user::space::{AddressSpace, MMAP_MIN_ADDR, SpaceError};

/// How much address space a program's stack gets.
///
/// Eight megabytes, which is what `RLIMIT_STACK` defaults to on Linux and
/// what a program's own guard-page arithmetic assumes. It costs nothing until
/// touched: the pages arrive on fault.
const STACK_SIZE: u64 = 8 * 1024 * 1024;

/// Address space kept inaccessible beneath the stack.
///
/// A mebibyte, which is Linux's `stack_guard_gap`. Reserved rather than
/// merely left free, because `mmap` searches for free space from the top of
/// the user half downwards and would otherwise place the next mapping flush
/// against the bottom of the stack -- where an overflow writes into it rather
/// than faulting.
const STACK_GUARD: u64 = 1024 * 1024;

/// The most the argument vector, environment and auxiliary vector may occupy.
///
/// Built in kernel memory and copied in, so this bounds a kernel allocation
/// rather than a user one. Linux's own limit is a quarter of the stack rlimit,
/// which would be two megabytes here. This is a quarter megabyte: enough for
/// `xargs` and `find -exec` building long command lines, which the first limit
/// of sixteen kilobytes was not, and still one allocation per `execve`.
const STARTUP_BYTES: usize = 256 * 1024;

/// The longest path `execve` accepts, Linux's `PATH_MAX`.
const PATH_MAX: usize = 4096;

/// The longest `#!` line read, Linux's `BINPRM_BUF_SIZE`.
const INTERPRETER_LINE: usize = 256;

/// The status a process ends with when `execve` fails after it has already
/// taken the old program's memory away: 128 plus `SIGSEGV`, which is what
/// Linux kills it with.
const LOST_STATUS: i32 = 128 + 11;

/// Why a program could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecError {
    /// The image could not be loaded.
    Load(LoadError),
    /// The address space refused the stack.
    Space(SpaceError),
    /// The startup image did not fit, or could not be written.
    Startup,
    /// The program was loaded, but its task could not be started.
    Start(&'static str),
    /// The dynamic linker the program names could not be read.
    Linker(Errno),
}

/// Where the stack goes: as high in the program's space as a page allows --
/// the user half, or below 4 GiB for a 32-bit program on a 64-bit kernel.
///
/// Below the space's end rather than at it, because the top page is left
/// unmapped deliberately — a program that walks off the end of its stack
/// should fault rather than wrap to zero.
fn stack_top(abi: Abi) -> u64 {
    (load::user_end(abi) - PAGE_SIZE) & !(ferrix_ustack::STACK_ALIGN - 1)
}

/// The program's pointer width, as `src/lib/kernel/ustack` wants it told: this build's
/// for the machine's own programs, and 32 bits for one in compatibility mode.
///
/// From the width rather than from a `cfg`, because that is what the question
/// actually is, and because generic kernel code naming an architecture is what
/// the layering check forbids.
fn width(abi: Abi) -> Width {
    if size_of::<usize>() == 8 && abi == Abi::Native {
        Width::Bits64
    } else {
        Width::Bits32
    }
}

/// A program to load, and the two names it goes by.
///
/// Two, because Linux keeps them apart and programs read both: `exe` is the
/// file that was actually loaded, absolute and with every symbolic link
/// resolved, which `/proc/<pid>/exe` reports and glibc's static startup
/// asserts is absolute; `exec_fn` is the filename the program was asked for
/// by, exactly as given, which `AT_EXECFN` points at. For a `#!` script the
/// first is the interpreter and the second the script.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Executable<'a> {
    /// The ELF image: bytes in memory, or a file whose headers have been read.
    pub(crate) image: Source<'a>,
    /// The absolute path of the file the image was read from.
    pub(crate) exe: &'a [u8],
    /// The filename it was asked for by.
    pub(crate) exec_fn: &'a [u8],
    /// The ids the file's set-user-id and set-group-id bits give it.
    pub(crate) set_ids: SetIds,
    /// The dynamic linker's image, when the program named one. Opened from
    /// the path its `PT_INTERP` holds, before anything is unmapped, because
    /// after the point of no return there is no program left to fail back to.
    pub(crate) interpreter: Option<Source<'a>>,
}

/// Load `image`, which came from no file, into a new process, ready to run
/// and not yet running.
///
/// With no file there is no path, so both names are the first argument, which
/// is what the boot checks have always been recorded as.
///
/// # Errors
///
/// [`ExecError`].
pub(crate) fn load(
    image: &[u8],
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<Arc<Process>, ExecError> {
    let name = args.first().copied().unwrap_or(b"");
    let program = Executable {
        image: Source::Bytes(image),
        exe: name,
        exec_fn: name,
        set_ids: SetIds::NONE,
        interpreter: None,
    };
    load_executable(program, args, env, random)
}

/// Load `image`, which came from no file, into a new process that starts with
/// nothing on its stack: a native program, told its bootstrap handle by its
/// start argument rather than by an argument vector.
///
/// It enters at the image's entry point with its stack pointer at
/// [`Image::stack_top`], and its start argument is zero until a
/// [`process::StartClaim`] starts it with one. `name` is what `/proc/<pid>/exe`
/// and its command line report. It runs as `credentials` from the first moment
/// anything can see it, and in `context` -- a creator's mount namespace, root
/// and working directory -- when one is given, where a process the kernel
/// starts is in `fs::root_disk::process_context`'s.
///
/// # Errors
///
/// [`ExecError`].
pub(crate) fn load_native(
    image: &[u8],
    name: &[u8],
    credentials: Credentials,
    context: Option<Context>,
) -> Result<Arc<Process>, ExecError> {
    load_native_in(image, name, credentials, context, None)
}

/// [`load_native`], numbered in `pids` and every namespace above it when the
/// creator is in a pid namespace below the first. A native process can make
/// Linux calls, so one left in the first namespace would name the machine's
/// processes by their kernel numbers from inside a container, and be out of
/// reach of its init's end (`docs/PIDNS.md` §4).
///
/// # Errors
///
/// [`ExecError`]; a namespace that is ending is out of memory.
pub(crate) fn load_native_in(
    image: &[u8],
    name: &[u8],
    credentials: Credentials,
    context: Option<Context>,
    pids: Option<&Arc<crate::syscall::pidns::PidNamespace>>,
) -> Result<Arc<Process>, ExecError> {
    let space = AddressSpace::new().map_err(ExecError::Space)?;
    let mut process =
        Process::new(Arc::clone(&space)).map_err(|_| ExecError::Space(SpaceError::OutOfMemory))?;
    if let Some(namespace) = pids {
        process
            .enter_pid_namespace(namespace)
            .map_err(|_| ExecError::Space(SpaceError::OutOfMemory))?;
    }
    process.with_credentials(|held| *held = credentials);
    if let Some(context) = context {
        // A creator's child: findable from `register` on, and its attributes
        // come from the creator in `launch`, after it.
        process.await_attributes();
        // Not shared with anything yet; what it replaces goes after the lock.
        let started_in = core::mem::replace(&mut *process.fs_context().lock(), context);
        drop(started_in);
    }
    let loaded = load_into(&space, Source::Bytes(image), None)?;
    // A native process's start argument is a 64-bit handle, and it speaks
    // the native ABI through `SYSCALL`: a 32-bit image cannot be one.
    if loaded.loaded.abi != Abi::Native {
        return Err(ExecError::Load(LoadError::WrongMachine(3)));
    }
    process.record_exec(name, None, &[name]);
    process.set_startup(Startup {
        entry: loaded.loaded.start,
        stack: loaded.stack_top,
        argument: 0,
        vdso: 0,
        abi: Abi::Native,
    });
    Ok(registry::register(process))
}

/// Load `program` into a new process, ready to run and not yet running.
///
/// # Errors
///
/// [`ExecError`].
pub(crate) fn load_executable(
    program: Executable<'_>,
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<Arc<Process>, ExecError> {
    load_as(Process::new, program, args, env, random)
}

/// [`load_executable`], making the process with `make`: [`Process::new`], or
/// [`Process::new_init`] for init's.
fn load_as(
    make: fn(Arc<AddressSpace>) -> Result<Process, AllocError>,
    program: Executable<'_>,
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<Arc<Process>, ExecError> {
    let space = AddressSpace::new().map_err(ExecError::Space)?;
    let process =
        make(Arc::clone(&space)).map_err(|_| ExecError::Space(SpaceError::OutOfMemory))?;
    let startup = populate(&space, &process, program, args, env, random)?;
    process.set_startup(startup);
    Ok(registry::register(process))
}

/// Open the dynamic linker `image` asks for, if it asks for one.
///
/// The program's `PT_INTERP` holds a path, and this is the one place that
/// turns it into a file. It is done before anything is unmapped, because there
/// is no way back from the point of no return: a linker that cannot be opened
/// has to be an `execve` that fails and leaves the caller running, not a
/// process killed halfway into being replaced.
///
/// Opened with [`crate::fs::open_program`], so the linker needs execute
/// permission exactly as the program does -- Linux opens it with `open_exec`
/// for the same reason. Its set-user-id bits are read and dropped: what a
/// program runs as is its own file's business, and a set-id linker would hand
/// every dynamic program its owner's identity.
///
/// # Errors
///
/// `ENOEXEC` for an image that asks for a linker without saying which, and
/// whatever resolving, opening or reading the path refuses.
pub(crate) fn linker_for(ctx: &Context, image: Source<'_>) -> Result<Option<ProgramFile>, Errno> {
    let path = match load::interpreter_of(image) {
        Ok(Some(path)) => path,
        Ok(None) => return Ok(None),
        Err(LoadError::Read(errno)) => return Err(errno),
        Err(_) => return Err(Errno::ENOEXEC),
    };
    let (file, _exe, _set_ids) = crate::fs::open_program(ctx, None, &path)?;
    Ok(Some(file))
}

/// An ELF image loaded into an empty address space, with its stack region
/// reserved: what a Linux program and a native process both start from.
#[derive(Debug)]
pub(crate) struct Image {
    /// Where the loader put it: the entry point, the program headers, and the
    /// end of its highest segment, above which the heap may grow.
    pub(crate) loaded: load::Loaded,
    /// The top of the reserved stack region, aligned as the ABI requires. A
    /// program with nothing on its stack starts with its stack pointer here.
    pub(crate) stack_top: u64,
}

/// Load `image` into `space`, which must be empty, and reserve its stack region
/// with a guard either side.
///
/// No startup image is built: [`populate`] adds Linux's -- arguments,
/// environment and auxiliary vector -- and a native process starts with
/// nothing on its stack, so it takes [`Image::stack_top`] as it is.
///
/// # Errors
///
/// [`ExecError`].
pub(crate) fn load_into(
    space: &AddressSpace,
    image: Source<'_>,
    interpreter: Option<Source<'_>>,
) -> Result<Image, ExecError> {
    let loaded = load::load(space, image, interpreter).map_err(ExecError::Load)?;

    // The stack region. Reserved whole; paid for a page at a time.
    let top = stack_top(loaded.abi);
    let low = top - STACK_SIZE;
    let _ = space
        .map_anonymous(low, STACK_SIZE, VmaFlags::READ_WRITE)
        .map_err(ExecError::Space)?;

    // Guard regions either side of the stack, with no access at all. Not
    // decoration: the first busybox run's first `mmap` landed at
    // 0x7FFFFFFFF000, the page this leaves unmapped above the stack, because
    // the free-space search runs top-down and that page was the highest hole.
    let _ = space
        .map_anonymous(top, load::user_end(loaded.abi) - top, VmaFlags::NONE)
        .map_err(ExecError::Space)?;
    if let Some(guard_low) = low.checked_sub(STACK_GUARD) {
        let _ = space
            .map_anonymous(guard_low, STACK_GUARD, VmaFlags::NONE)
            .map_err(ExecError::Space)?;
    }
    Ok(Image {
        loaded,
        stack_top: top,
    })
}

/// Load `image` into `space`, which must be empty, with [`load_into`], and
/// build Linux's startup stack: what a new process and `execve` share.
///
/// # Errors
///
/// [`ExecError`].
fn populate(
    space: &AddressSpace,
    process: &Process,
    program: Executable<'_>,
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<Startup, ExecError> {
    let image = load_into(space, program.image, program.interpreter)?;
    let loaded = &image.loaded;
    let top = image.stack_top;
    process.set_heap_base(loaded.end);

    // Build the startup image in kernel memory, then copy it in. It cannot be
    // built in place: `src/lib/kernel/ustack` needs a `&mut [u8]` and the only way to
    // reach user memory is through the copy layer, one page at a time.
    let mut scratch = vec![0_u8; STARTUP_BYTES];
    let base = top - STARTUP_BYTES as u64;
    // What the processor can do. Not optional on Arm: musl's ARMv7-A `setjmp`
    // saves the callee-saved double registers only when told there is a VFP.
    let (hwcap, hwcap2) = arch::user_hwcaps();
    // The ids the process runs the program with, as `Credentials::exec`
    // leaves them: a set-user-id or set-group-id file's owner, and
    // `AT_SECURE` when what it starts with is not what it had.
    let (user, group, secure) = process.with_credentials(|credentials| {
        let secure = credentials.exec(program.set_ids.uid, program.set_ids.gid);
        (credentials.user, credentials.group, secure)
    });
    // The vDSO, where there is one: `AT_SYSINFO_EHDR` is how the C library
    // finds it, and a program started without the entry makes the system
    // calls the vDSO would have answered. Not for a 32-bit program: the vDSO
    // is a 64-bit image its libc could not read, and both musl and glibc
    // make those calls themselves without it (`docs/I386.md` §2).
    let vdso = match loaded.abi {
        Abi::Native => super::vdso::map_into(space),
        Abi::Compat => None,
    };
    // ARMv7-A's signal return page, where a handler without `SA_RESTORER`
    // goes back: mapped as the vDSO is, and like it given up rather than
    // failing the exec if there is no room. Named in no auxiliary vector
    // entry, as Linux's `sigpage` is not.
    if loaded.abi == Abi::Native {
        let _ = super::sigpage::map_into(space);
    }
    let auxv = [
        (AT_SYSINFO_EHDR, vdso.unwrap_or(0)),
        (AT_HWCAP, hwcap),
        (AT_HWCAP2, hwcap2),
        (AT_PAGESZ, PAGE_SIZE),
        (AT_PHDR, loaded.phdr),
        (AT_PHENT, loaded.phent),
        (AT_PHNUM, loaded.phnum),
        (AT_ENTRY, loaded.entry),
        // Where the dynamic linker was placed, and zero when there is none.
        // A linker reads it to find its own segments before it can relocate
        // itself; musl's and glibc's both refuse to start without it. Zero is
        // the right answer for a static program and is what Linux gives one.
        (AT_BASE, loaded.base),
        (AT_UID, u64::from(user.real)),
        (AT_EUID, u64::from(user.effective)),
        (AT_GID, u64::from(group.real)),
        (AT_EGID, u64::from(group.effective)),
        (AT_SECURE, u64::from(secure)),
        (AT_CLKTCK, 100),
    ];
    let exec_fn = program.exec_fn;
    let spec = Spec {
        args,
        env,
        auxv: auxv.get(usize::from(vdso.is_none())..).unwrap_or_default(),
        random,
        exec_fn,
        platform: arch::user_platform(loaded.abi),
        width: width(loaded.abi),
    };
    let startup = ferrix_ustack::build(&spec, top, &mut scratch).map_err(|_| ExecError::Startup)?;
    uaccess::copy_to_user(space, base, &scratch).map_err(|_| ExecError::Startup)?;

    // The file, where there is one, is what `/proc/<pid>/exe` leads to: for a
    // script, the interpreter's, as on Linux.
    let exe_at = match program.image {
        Source::File(file) => Some(file.file().location().clone()),
        Source::Bytes(_) => None,
    };
    process.record_exec(program.exe, exe_at, args);
    Ok(Startup {
        entry: loaded.start,
        stack: startup.sp,
        argument: 0,
        vdso: vdso.unwrap_or(0),
        abi: loaded.abi,
    })
}

/// Load `image`, run it as a task of its own, and wait for it to end.
///
/// Returns the status the program exited with. The caller blocks for as long
/// as the program runs, which is the point for the first program and for the
/// checks; everything else wants [`load`] and [`process::start`] separately.
///
/// # Errors
///
/// [`ExecError`].
pub(crate) fn run(
    image: &[u8],
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<i32, ExecError> {
    let name = args.first().copied().unwrap_or(b"");
    let program = Executable {
        image: Source::Bytes(image),
        exe: name,
        exec_fn: name,
        set_ids: SetIds::NONE,
        interpreter: None,
    };
    run_executable(program, args, env, random)
}

/// [`run`], for a program that names a dynamic linker, with the linker
/// supplied.
///
/// For the boot check, which has both images in hand and wants the whole path
/// exercised without an `execve` to carry them.
///
/// # Errors
///
/// [`ExecError`].
pub(crate) fn run_with_linker(
    image: &[u8],
    linker: Source<'_>,
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<i32, ExecError> {
    let name = args.first().copied().unwrap_or(b"");
    let program = Executable {
        image: Source::Bytes(image),
        exe: name,
        exec_fn: name,
        set_ids: SetIds::NONE,
        interpreter: Some(linker),
    };
    run_executable(program, args, env, random)
}

/// [`run`], for a program read from a file and named by it.
///
/// # Errors
///
/// [`ExecError`].
pub(crate) fn run_executable(
    program: Executable<'_>,
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<i32, ExecError> {
    run_as(Process::new, program, args, env, random)
}

/// How many times [`run_init`] looks for pid 1 to come free, and how long it
/// sleeps between looks: a second in all.
const INIT_PID_WAITS: u32 = 200;
/// See [`INIT_PID_WAITS`].
const INIT_PID_WAIT_NANOS: u64 = 5_000_000;

/// [`run_executable`], as init: the process gets pid 1, as the first user
/// process does on Linux, and `bootstrap`, when there is one, waiting for it
/// to take with `process_bootstrap` (`docs/INIT.md` §6, K2).
///
/// A list of commands runs its programs one after another, each as init in
/// turn, and the last one's pid can still be held for a moment by its task on
/// the way to being reaped. That is waited out, briefly, before settling for
/// another pid, which only a process that outlived its program would force.
///
/// # Errors
///
/// [`ExecError`]; `bootstrap` is closed.
pub(crate) fn run_init(
    program: Executable<'_>,
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
    bootstrap: Option<Transfer>,
) -> Result<i32, ExecError> {
    for _ in 0..INIT_PID_WAITS {
        if registry::is_free(registry::INIT_PID) {
            break;
        }
        crate::sched::sleep_for(INIT_PID_WAIT_NANOS);
    }
    let process = match load_as(Process::new_init, program, args, env, random) {
        Ok(process) => process,
        Err(problem) => {
            object::dispose(bootstrap.map(|(object, _)| object));
            return Err(problem);
        }
    };
    give_bootstrap(&process, bootstrap);
    let task = match process::start(&process) {
        Ok(task) => task,
        Err(problem) => {
            object::dispose(process.close_bootstrap());
            return Err(ExecError::Start(problem));
        }
    };
    drop(task);
    process
        .wait_for_exit(u64::MAX)
        .ok_or(ExecError::Start("the program never reported how it ended"))
}

/// Hold `bootstrap` in `process`'s bootstrap slot, for its program to take
/// with `process_bootstrap`: what the kernel gives pid 1 (K2). A process
/// already given one, or past its `execve`, is refused, and the object is
/// closed.
pub(crate) fn give_bootstrap(process: &Process, bootstrap: Option<Transfer>) {
    let Some((object, rights)) = bootstrap else {
        return;
    };
    if let Err((_, object)) = process.with_bootstrap(|slot| slot.give(object, rights)) {
        object::dispose([object]);
    }
}

/// Load `program` with [`load_as`], run it, and wait for it to end.
fn run_as(
    make: fn(Arc<AddressSpace>) -> Result<Process, AllocError>,
    program: Executable<'_>,
    args: &[&[u8]],
    env: &[&[u8]],
    random: [u8; ferrix_ustack::RANDOM_BYTES],
) -> Result<i32, ExecError> {
    let process = load_as(make, program, args, env, random)?;
    let _task = process::start(&process).map_err(ExecError::Start)?;
    process
        .wait_for_exit(u64::MAX)
        .ok_or(ExecError::Start("the program never reported how it ended"))
}

/// How `execve` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecveError {
    /// Refused before anything changed; the caller returns this error.
    Refused(Errno),
    /// Failed after the old program's memory was already gone, so there is
    /// nothing to return to: the caller ends the process.
    Lost,
}

impl From<Errno> for ExecveError {
    fn from(error: Errno) -> Self {
        ExecveError::Refused(error)
    }
}

/// `execve`: replace the running program with the one at `path`.
///
/// Answers the new program's entry point and stack pointer, for the trap path
/// to enter.
///
/// # The point of no return
///
/// Everything that can be refused is refused first -- the path, the argument
/// and environment strings, the file, a `#!` interpreter, and the ELF headers
/// -- because until then a failure is an error the old program can handle.
/// Then the old program's memory goes, and a failure from there on ends the
/// process, as it does on Linux: loading into the emptied space rather than
/// building a new one keeps the process's identity -- its address space,
/// its task, everything that holds a reference to either -- exactly as it was.
///
/// # Errors
///
/// [`ExecveError`].
pub(crate) fn sys_execve(
    process: &Process,
    path: u64,
    argv: u64,
    envp: u64,
    word: usize,
) -> Result<(u64, u64, Abi), ExecveError> {
    execve_at(process, AT_FDCWD, path, argv, envp, 0, word)
}

/// `execveat`: [`sys_execve`], with a relative path resolved from the
/// directory `dirfd` names.
///
/// `AT_EMPTY_PATH` with an empty path runs the file `dirfd` itself names,
/// which is how `fexecve` is built. `AT_SYMLINK_NOFOLLOW` refuses a path whose
/// last component is a symbolic link with `ELOOP`. Any other flag is `EINVAL`.
///
/// # Errors
///
/// [`ExecveError`].
pub(crate) fn sys_execveat(
    process: &Process,
    dirfd: i32,
    path: u64,
    argv: u64,
    envp: u64,
    flags: u32,
    word: usize,
) -> Result<(u64, u64, Abi), ExecveError> {
    execve_at(process, dirfd, path, argv, envp, flags, word)
}

/// The one body of `execve` and `execveat`. See [`sys_execve`]. `word` is
/// the width of the caller's `argv` and `envp` pointers: four for a 32-bit
/// program, whatever the program it runs will be.
fn execve_at(
    process: &Process,
    dirfd: i32,
    path: u64,
    argv: u64,
    envp: u64,
    flags: u32,
    word: usize,
) -> Result<(u64, u64, Abi), ExecveError> {
    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL.into());
    }
    let space = process.space();
    let mut path_bytes = Vec::new();
    uaccess::copy_cstr_from_user(space, path, PATH_MAX, &mut path_bytes)
        .map_err(|_| Errno::EFAULT)?;
    if path_bytes.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(Errno::ENOENT.into());
    }

    let mut budget = STARTUP_BYTES;
    let mut args = read_strings(space, argv, word, &mut budget)?;
    let env = read_strings(space, envp, word, &mut budget)?;

    // The caller's own root and working directory, so a relative path after
    // `cd` resolves from there, and its identity, which the walk and the
    // execute check are made as. Cloned out: never walk with the lock held.
    let context = crate::syscall::path::context(process);
    let (mut image, mut exe, mut set_ids) = if path_bytes.is_empty() {
        let file = fd::file(process, dirfd)?;
        let image = open_descriptor(&file, &context.who)?;
        let set_ids = crate::fs::set_ids_on(file.location(), &file.inode().metadata());
        let exe = crate::fs::namespace().path_of(file.location(), &context.root);
        path_bytes = descriptor_path(dirfd, &[]);
        (image, exe, set_ids)
    } else {
        let start = fd::start_for(process, dirfd, &path_bytes)?;
        if flags & AT_SYMLINK_NOFOLLOW != 0 {
            let ns = crate::fs::namespace();
            let at = ns.resolve(&context, start.as_ref(), &path_bytes, false)?;
            if ns.stat(&at)?.metadata.kind == FileType::Symlink {
                return Err(Errno::ELOOP.into());
            }
        }
        let (image, exe, set_ids) = crate::fs::open_program(&context, start.as_ref(), &path_bytes)?;
        // What a script's interpreter is handed as the script's name, and
        // what `AT_EXECFN` names: the path itself when it means the same
        // thing from anywhere, and a name through the directory descriptor
        // when it does not, as Linux's `do_execveat_common` builds it.
        if start.is_some() {
            path_bytes = descriptor_path(dirfd, &path_bytes);
        }
        (image, exe, set_ids)
    };
    // The filename as asked for, which a script keeps: `AT_EXECFN` is the
    // script's name even though the interpreter is what runs.
    let exec_fn = path_bytes.clone();

    // A script names its interpreter on its first line, which runs with the
    // script's path in place of its own first argument -- what Linux's
    // `binfmt_script` does. One level: an interpreter that is itself a script
    // is refused rather than followed.
    if image.head().starts_with(b"#!") {
        let (interpreter, argument) = interpreter_line(image.head())?;
        let mut replaced = Vec::with_capacity(args.len().saturating_add(2));
        replaced.push(interpreter.clone());
        if let Some(argument) = argument {
            replaced.push(argument);
        }
        replaced.push(path_bytes);
        replaced.extend(args.into_iter().skip(1));
        args = replaced;
        // The interpreter is the file actually loaded, so it is the exe -- and
        // its set-id bits are the ones that count, which is why a set-user-id
        // script gives nothing away here, as it gives nothing away on Linux.
        (image, exe, set_ids) = crate::fs::open_program(&context, None, &interpreter)?;
        if image.head().starts_with(b"#!") {
            return Err(Errno::ENOEXEC.into());
        }
    }
    // Before the point of no return: the linker is opened here so that a program
    // naming one that is missing or unreadable is an `execve` that fails, with
    // the caller still running the program it had.
    let linker = linker_for(&context, Source::File(&image))?;
    load::check(Source::File(&image), linker.as_ref().map(Source::File)).map_err(refused)?;

    let arg_slices: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
    let env_slices: Vec<&[u8]> = env.iter().map(Vec::as_slice).collect();

    // Every other thread ends first, as Linux's `de_thread` ends them: each
    // would otherwise run on in the memory about to be replaced, and write its
    // cleared id into the new program as it ended. The caller takes the pid if
    // it was not the first thread. Refused only when another thread is already
    // replacing the program or the process is ending, and then this thread
    // never returns to the program. A self-check that execs into a process it
    // does not run in has no thread there to keep.
    if let Some(thread) = crate::syscall::thread::current_of(process) {
        process.end_other_threads(&thread)?;
    }

    // The point of no return.
    empty_user_half(space).map_err(|_| ExecveError::Lost)?;
    process.reset_for_exec();
    crate::syscall::attributes::forget_robust_list(process);
    // The address the calling thread asked to have cleared, and its alternate
    // stack, were in the memory just emptied. Only the caller's own thread: a
    // self-check can exec into a process it is not running in.
    if let Some(thread) = crate::syscall::thread::current()
        && core::ptr::eq(Arc::as_ptr(thread.process()), process)
    {
        let _ = thread.take_clear_child_tid();
        thread.with_own_signals(crate::syscall::signal::ThreadSignals::reset_for_exec);
    }
    // `PR_SET_NO_NEW_PRIVS` gives up set-id programs for good, as it does on
    // Linux: the file's bits are read and then dropped.
    if crate::syscall::attributes::get(process).no_new_privs {
        set_ids = SetIds::NONE;
    }
    // The caller's ids, which the new program's dumpability is judged by.
    let ids_before = crate::syscall::attributes::ids_before_exec(process);
    let program = Executable {
        image: Source::File(&image),
        exe: &exe,
        exec_fn: &exec_fn,
        set_ids,
        interpreter: linker.as_ref().map(Source::File),
    };
    let startup = populate(
        space,
        process,
        program,
        &arg_slices,
        &env_slices,
        random_bytes(),
    )
    .map_err(|_| ExecveError::Lost)?;
    process.set_startup(startup);
    crate::syscall::attributes::exec_dumpable(process, ids_before);

    // Descriptors marked close-on-exec go, dropped after the table's lock is
    // let go, since closing one can wake whatever waits on it. And a `vfork`
    // parent, asleep since the fork, may run again.
    let closed = process.files().lock().take_cloexec();
    for file in &closed {
        fd::closed(process, file);
    }
    drop(closed);
    let _ = crate::fs::socket::collect_cycles();
    process.mark_execed();

    // The old program's thread pointer and floating-point state are its own
    // and must not reach the new one. The registers are still live on this
    // processor, inside this task's own system call.
    // SAFETY: called by the user task whose registers these are.
    unsafe { arch::reset_user_state() };

    Ok((startup.entry, startup.stack, startup.abi))
}

/// What `execve` answers for an image the loader refuses before the point of
/// no return: the error reading the file met, and `ENOEXEC` for anything
/// about the image itself.
fn refused(error: LoadError) -> Errno {
    match error {
        LoadError::Read(errno) => errno,
        _ => Errno::ENOEXEC,
    }
}

/// What a failed `execve` past its point of no return ends the process with.
pub(crate) const fn lost_status() -> i32 {
    LOST_STATUS
}

/// The regular file `file` names, opened as a program, for `execveat` with
/// `AT_EMPTY_PATH`.
///
/// Read through the description itself when it was opened for reading, at
/// explicit offsets so the caller's file position is left alone, which also
/// works for a file unlinked since it was opened. A description that cannot
/// read -- `O_PATH`, which is what `fexecve` is usually given, or write-only
/// -- is opened afresh from where it points, since running a file needs no
/// read access through the descriptor on Linux either.
///
/// # Errors
///
/// `EACCES` for anything but a regular file, as Linux answers, for one on a
/// `noexec` mount, and for one `who` may not execute; and whatever reopening
/// or reading refuses.
fn open_descriptor(file: &Arc<OpenFile>, who: &Access) -> Result<ProgramFile, Errno> {
    if file.kind() != FileType::Regular || file.location().mount.no_exec() {
        return Err(Errno::EACCES);
    }
    who.require(&file.inode().metadata(), MAY_EXEC)?;
    let reader = if file.readable() {
        Arc::clone(file)
    } else {
        let flags = OpenFlags {
            read: true,
            ..OpenFlags::default()
        };
        OpenFile::new(file.location().clone(), &flags)?
    };
    ProgramFile::open(reader)
}

/// The name a script run through `execveat` is handed to its interpreter
/// under: `/dev/fd/<dirfd>`, followed by the relative path if there is one.
///
/// Linux's convention, and the only name that means the right file from
/// wherever the interpreter runs. Whether it can then be opened depends on
/// `/dev/fd`, which is the same condition Linux sets.
fn descriptor_path(dirfd: i32, path: &[u8]) -> Vec<u8> {
    let mut name = alloc::format!("/dev/fd/{dirfd}").into_bytes();
    if !path.is_empty() {
        name.push(b'/');
        name.extend_from_slice(path);
    }
    name
}

/// Take every mapping out of the user half.
///
/// From [`MMAP_MIN_ADDR`] up to the space's ceiling, because nothing can be
/// mapped outside that window and the map refuses a range that reaches past
/// it -- which a 32-bit program's lower ceiling would make of the whole user
/// half.
fn empty_user_half(space: &AddressSpace) -> Result<(), SpaceError> {
    space.unmap(MMAP_MIN_ADDR, space.ceiling() - MMAP_MIN_ADDR)
}

/// Read a `NULL`-terminated array of string pointers from the program, as
/// `argv` and `envp` are passed, `word` bytes a pointer, charging each string
/// to `budget`.
///
/// A null array is an empty one, which Linux accepts for both.
fn read_strings(
    space: &AddressSpace,
    at: u64,
    word: usize,
    budget: &mut usize,
) -> Result<Vec<Vec<u8>>, Errno> {
    let mut strings = Vec::new();
    if at == 0 {
        return Ok(strings);
    }
    let stride = word as u64;
    let mut slot = at;
    loop {
        let mut bytes = [0_u8; 8];
        let target = bytes.get_mut(..word).ok_or(Errno::EINVAL)?;
        uaccess::copy_from_user(space, slot, target).map_err(|_| Errno::EFAULT)?;
        let pointer = u64::from_le_bytes(bytes);
        if pointer == 0 {
            return Ok(strings);
        }
        let mut string = Vec::new();
        uaccess::copy_cstr_from_user(space, pointer, ferrix_ustack::MAX_ARG_STRLEN, &mut string)
            .map_err(|_| Errno::EFAULT)?;
        // The string, its terminator and its pointer, which is what it costs
        // on the new program's stack.
        let cost = string.len().saturating_add(1).saturating_add(word);
        *budget = budget.checked_sub(cost).ok_or(Errno::E2BIG)?;
        strings.push(string);
        slot = slot.checked_add(stride).ok_or(Errno::EFAULT)?;
    }
}

/// The interpreter and its one optional argument from a `#!` line: what
/// `execve` runs a script with, and what init does when `ferrix.init=` names
/// one.
pub(crate) fn interpreter_line(image: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>), Errno> {
    let line = image.get(2..).ok_or(Errno::ENOEXEC)?;
    let end = line
        .iter()
        .take(INTERPRETER_LINE)
        .position(|&byte| byte == b'\n')
        .unwrap_or_else(|| line.len().min(INTERPRETER_LINE));
    let line = line.get(..end).ok_or(Errno::ENOEXEC)?;
    let line = line.trim_ascii();
    let split = line
        .iter()
        .position(u8::is_ascii_whitespace)
        .unwrap_or(line.len());
    let (interpreter, rest) = line.split_at(split);
    if interpreter.is_empty() {
        return Err(Errno::ENOEXEC);
    }
    let rest = rest.trim_ascii();
    let argument = (!rest.is_empty()).then(|| rest.to_vec());
    Ok((interpreter.to_vec(), argument))
}

/// Sixteen bytes for `AT_RANDOM`, from the kernel's generator, which is
/// what a C library seeds its stack protector and pointer guard from.
pub(crate) fn random_bytes() -> [u8; ferrix_ustack::RANDOM_BYTES] {
    let mut bytes = [0_u8; ferrix_ustack::RANDOM_BYTES];
    crate::random::fill(&mut bytes);
    bytes
}
