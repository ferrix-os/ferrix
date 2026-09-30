//! The self-check of the kernel calls init needs beyond stage 13
//! (`docs/INIT.md` §11): K2's bootstrap channel for pid 1, and K3's
//! `process_give` and `process_bootstrap`.
//!
//! **K2.** The channel `init::run` gives each program it starts is made by
//! [`init::bootstrap_channel`], and given as `exec::run_init` gives it. Given
//! so to a process of the check's own, `process_bootstrap` must answer a
//! channel end on which exactly one message waits: the kernel's hello,
//! [`INIT_HELLO_BYTES`] bytes with no handle, which `src/lib/proto/native-abi`
//! recognises as version 1, with the kernel's end still open. Given so to a
//! loaded program, the program must take it by number and close it, and the
//! kernel's end must hear the close.
//!
//! **K4.** `port_fd` by number on a port gives a descriptor, close-on-exec
//! when asked, that polls not readable while the port is empty. An
//! `epoll_wait` on it, with five seconds to wait, is ended by a packet a
//! kernel task queues a moment later -- by the port's queue waking it, and
//! well before the wait's own one-second look -- and reports `EPOLLIN` with
//! the registration's cookie. Once the packet is taken with `port_wait` the
//! descriptor is quiet again. A `read` is `EINVAL`, and the descriptor keeps
//! the port after its handle is closed. Refused for a flag it does not know,
//! a handle that is not a port, and one without `WAIT`.
//!
//! **K6.** `process_status` by number on handles to processes: running
//! before anything ends it, then killed by the signal a kill named, `SIGKILL`
//! for a job's kill, and exited with the code a program exited with; and
//! refused for a handle without `WAIT`, one that is not a process, and an
//! answer nobody can write.
//!
//! **K3, the calls.** Driven through [`native::dispatch`] by number, between a
//! process and a fork of it, as the object checks drive the native calls. A
//! give moves the handle, not a copy of it: the parent's number names nothing
//! after it, and the child's `process_bootstrap` answers a handle to the same
//! channel end with the same rights, once, and zero after. It is refused to a
//! process that is not the caller's child, to a pid that names nothing, for a
//! handle without `TRANSFER`, a second time, to a child that has completed an
//! `execve` with nothing given, and to one that has ended -- each refusal
//! leaving the handle with the caller. A handle given before the `execve`
//! outlives it, and one never taken is closed when the child ends, which the
//! peer end hears.
//!
//! **K3, from a program.** A fork of a loaded program is given a channel end
//! and started; it `execve`s a program from `/tmp` that takes its bootstrap by
//! number, closes it, and exits with what the close answered.
//! It must exit 0, and the kept end must hear the close. The parent, given
//! nothing, then runs the same `execve` and must exit with the close's
//! `EBADF`: `process_bootstrap` answered zero.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{EPOLL_CTL_ADD, EPOLLIN, F_GETFD, FD_CLOEXEC};
use ferrix_native_abi::bootstrap::{
    INIT_HELLO_BYTES, INIT_HELLO_HANDLES, INIT_HELLO_VERSION, init_hello_version,
};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{PORT_FD_CLOEXEC, PROCESS_EXITED, PROCESS_KILLED, PROCESS_RUNNING};
use ferrix_vfs::OpenFlags;

use crate::arch;
use crate::init;
use crate::object::channel::Endpoint;
use crate::object::check::{SCRATCH, Side};
use crate::object::port::Port;
use crate::object::process::ProcessRef;
use crate::object::{self, Object};
use crate::sync::SpinLock;
use crate::syscall::epoll::{self as epoll_calls, EVENT_BYTES};
use crate::syscall::process::{self, Process};
use crate::syscall::{check as syscall_check, exec, fd, file, image, native};
use crate::trap::SyscallArgs;

/// The path [`arch::USER_EXEC_PROGRAM`] carries at its end, on every
/// architecture, NUL included.
const EXEC_PATH: &[u8] = b"/exec-target\0";

/// The path this check has it run instead: the same length, so the program
/// is patched without knowing its instruction set, as stage 7's spinning
/// program is; and under `/tmp`, which is a tmpfs on every boot, since by the
/// time this runs `/` may be a disk's root.
const EXEC_TARGET: &[u8] = b"/tmp/k3-exec\0";

/// How long a program run here is given to end.
const PATIENCE_NANOS: u64 = 30_000_000_000;

/// What [`arch::USER_BOOTSTRAP_PROGRAM`] exits with when `process_bootstrap`
/// answered zero: the close's `-EBADF`, as `exit_group` keeps its low byte.
const NOTHING_STATUS: i32 = -(Errno::EBADF.0 as i32) & 0xFF;

/// What the check did, for the boot log.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Calls refused as the ABI says.
    pub(crate) refusals: u32,
    /// Bootstrap handles given and taken.
    pub(crate) taken: u32,
    /// Whether a program took its bootstrap after an `execve`; `false` on an
    /// architecture with no program to run.
    pub(crate) from_a_program: bool,
    /// The version of the hello read from init's bootstrap channel.
    pub(crate) hello: u32,
    /// Ends read back through process handles.
    pub(crate) statuses: u32,
    /// How long, in milliseconds, an `epoll_wait` on a port's descriptor
    /// took to be woken by a packet queued 50 ms into it.
    pub(crate) woken_after_ms: u64,
}

/// Run every check.
///
/// # Errors
///
/// The first property that did not hold, as a sentence.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report {
        hello: check_the_kernel_greets_init()?,
        ..Report::default()
    };
    check_init_takes_its_channel()?;
    check_process_status(&mut report)?;
    report.woken_after_ms = check_port_descriptor(&mut report)?;
    check_give_and_take(&mut report)?;
    report.from_a_program = check_a_program_takes_it_after_execve()?;
    Ok(report)
}

/// Where the hello is read to, the handles it carries, and what the read
/// reports, in a [`Side`]'s scratch region.
const HELLO_AT: u64 = SCRATCH + 0x600;
const HELLO_HANDLES_AT: u64 = SCRATCH + 0x700;
const HELLO_ACTUAL_AT: u64 = SCRATCH + 0x800;

/// Room offered for the hello: more than version 1 needs, so a longer
/// message would be read whole and seen.
const HELLO_ROOM: u64 = 64;

/// K2: init's bootstrap, given as pid 1 is given it, holds one message, the
/// kernel's hello, and its peer is open. The version read.
fn check_the_kernel_greets_init() -> Result<u32, &'static str> {
    let (kernel_end, program_end) =
        init::bootstrap_channel().ok_or("no memory for init's bootstrap channel")?;
    let side = Side::new()?;
    exec::give_bootstrap(
        &side.process,
        Some((Object::Channel(program_end), Rights::CHANNEL)),
    );
    let outcome = read_the_hello(&side);
    side.close_everything();
    drop(kernel_end);
    outcome
}

/// The body of [`check_the_kernel_greets_init`].
fn read_the_hello(side: &Side) -> Result<u32, &'static str> {
    let taken = side
        .call(nr::PROCESS_BOOTSTRAP, &[])
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .map(Handle)
        .filter(|handle| handle.is_valid())
        .ok_or("process_bootstrap in a process given init's channel answered no handle")?;
    let read = side.call(
        nr::CHANNEL_READ,
        &[
            reg(taken),
            HELLO_AT,
            HELLO_ROOM,
            HELLO_HANDLES_AT,
            4,
            HELLO_ACTUAL_AT,
        ],
    );
    if read != Ok(0) {
        return Err("init's bootstrap channel held no message for it to read");
    }
    let bytes = side.get_u32(HELLO_ACTUAL_AT)? as usize;
    let handles = side.get_u32(HELLO_ACTUAL_AT + 4)? as usize;
    let message = side.get(HELLO_AT, bytes)?;
    let version = init_hello_version(&message);
    if bytes != INIT_HELLO_BYTES || handles != INIT_HELLO_HANDLES {
        crate::console::println!(
            "  initcall init's first message was {bytes} bytes and {handles} handles"
        );
        return Err("the first message on init's bootstrap channel is not version 1's shape");
    }
    if version != Some(INIT_HELLO_VERSION) {
        return Err("the first message on init's bootstrap channel is not the kernel's hello");
    }
    if side.call(
        nr::CHANNEL_READ,
        &[
            reg(taken),
            HELLO_AT,
            HELLO_ROOM,
            HELLO_HANDLES_AT,
            4,
            HELLO_ACTUAL_AT,
        ],
    ) != Err(status::SHOULD_WAIT)
    {
        return Err("init's bootstrap channel held more than the one message");
    }
    let open = side.process.with_handles(|table| match table.get(taken) {
        Ok((Object::Channel(end), _)) => !end.signals().intersects(Signals::PEER_CLOSED),
        _ => false,
    });
    if !open {
        return Err("the kernel's end of init's bootstrap channel was closed");
    }
    version.ok_or("the first message on init's bootstrap channel is not the kernel's hello")
}

/// K2, from a program: one given init's channel as pid 1 is takes it by
/// number and closes it, and the kernel's end hears.
fn check_init_takes_its_channel() -> Result<(), &'static str> {
    if arch::USER_BOOTSTRAP_PROGRAM.is_empty() {
        return Ok(());
    }
    let class = if size_of::<usize>() == 8 {
        ferrix_elf::Class::Elf64
    } else {
        ferrix_elf::Class::Elf32
    };
    let program = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_BOOTSTRAP_PROGRAM,
    );
    let (kernel_end, program_end) =
        init::bootstrap_channel().ok_or("no memory for init's bootstrap channel")?;
    let process = exec::load(
        &program,
        &[b"/init-bootstrap"],
        &[],
        [0x4c; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program to take init's channel could not be loaded")?;
    exec::give_bootstrap(
        &process,
        Some((Object::Channel(program_end), Rights::CHANNEL)),
    );
    let status = run_to_its_end(&process)?;
    if status != 0 {
        crate::console::println!("  initcall a program given init's channel exited with {status}");
        return Err("a program given init's bootstrap channel did not take and close it");
    }
    if !kernel_end.signals().intersects(Signals::PEER_CLOSED) {
        return Err("the kernel's end of init's channel did not hear the program close its end");
    }
    Ok(())
}

/// Where `process_status` writes, in a [`Side`]'s scratch region.
const STATUS_AT: u64 = SCRATCH + 0x900;

/// A user address nothing maps: the scratch region is two pages.
const UNMAPPED: u64 = SCRATCH + 0x10_0000;

/// K6: how a process ended, read through a handle to it.
fn check_process_status(report: &mut Report) -> Result<(), &'static str> {
    let side = Side::new()?;
    let outcome = process_statuses(&side, report);
    side.close_everything();
    outcome
}

/// A handle in `side` to `process`, with `rights`.
fn process_handle(side: &Side, process: &Process, rights: Rights) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Process(ProcessRef::new(process)), rights))
        .map_err(|_| "no room for a handle to a process")
}

/// `process_status` on `handle` in `side`: the state and the value.
fn status_of(side: &Side, handle: Handle) -> Result<(u32, u32), &'static str> {
    side.put(STATUS_AT, &[0xA5; 8])?;
    if side.call(nr::PROCESS_STATUS, &[reg(handle), STATUS_AT]) != Ok(0) {
        return Err("process_status on a handle to a process was refused");
    }
    Ok((side.get_u32(STATUS_AT)?, side.get_u32(STATUS_AT + 4)?))
}

/// The body of [`check_process_status`].
///
/// Verifies: L.object.94
fn process_statuses(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let signalled = process::new_for_check().map_err(|_| "could not make a process")?;
    let jobbed = process::new_for_check().map_err(|_| "could not make a process")?;
    let on_signal = process_handle(side, &signalled, Rights::PROCESS)?;
    let on_job = process_handle(side, &jobbed, Rights::PROCESS)?;
    let blind = process_handle(side, &signalled, Rights::DUPLICATE)?;

    if status_of(side, on_signal)? != (PROCESS_RUNNING, 0) {
        return Err("process_status of a process nothing has ended did not say running");
    }
    let sigterm = ferrix_linux_abi::types::SIGTERM;
    process::kill(&signalled, 128 + sigterm as i32);
    process::kill(&jobbed, object::job::KILLED_STATUS);
    let killed = status_of(side, on_signal)?;
    if killed != (PROCESS_KILLED, sigterm) {
        crate::console::println!(
            "  initcall a process ended by SIGTERM read as state {} value {}",
            killed.0,
            killed.1
        );
        return Err("process_status of a process a signal ended did not say killed, by it");
    }
    if status_of(side, on_job)? != (PROCESS_KILLED, ferrix_linux_abi::types::SIGKILL) {
        return Err("process_status of a process its job's kill ended did not say SIGKILL");
    }
    report.statuses += 2;

    let (channel, ..) = channel_in(&side.process, Rights::CHANNEL)?;
    for (result, wanted, what) in [
        (
            side.call(nr::PROCESS_STATUS, &[reg(blind), STATUS_AT]),
            status::ACCESS_DENIED,
            "process_status through a handle without WAIT was not refused with ACCESS_DENIED",
        ),
        (
            side.call(nr::PROCESS_STATUS, &[reg(channel), STATUS_AT]),
            status::WRONG_TYPE,
            "process_status on a channel was not refused with WRONG_TYPE",
        ),
        (
            side.call(nr::PROCESS_STATUS, &[reg(on_signal), UNMAPPED]),
            status::FAULT,
            "process_status into memory nobody maps was not refused with FAULT",
        ),
    ] {
        refused(result, wanted, what, report)?;
    }

    if arch::USER_TEST_PROGRAM.is_empty() {
        return Ok(());
    }
    let class = if size_of::<usize>() == 8 {
        ferrix_elf::Class::Elf64
    } else {
        ferrix_elf::Class::Elf32
    };
    let program = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_TEST_PROGRAM,
    );
    let exiting = exec::load(
        &program,
        &[b"/exits"],
        &[],
        [0x4d; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that exits could not be loaded")?;
    let on_exit = process_handle(side, &exiting, Rights::PROCESS)?;
    let code = run_to_its_end(&exiting)?;
    let exited = status_of(side, on_exit)?;
    if exited != (PROCESS_EXITED, code.cast_unsigned()) {
        crate::console::println!(
            "  initcall a program that exited with {code} read as state {} value {}",
            exited.0,
            exited.1
        );
        return Err("process_status of a program that exited did not say exited, with its code");
    }
    report.statuses += 1;
    Ok(())
}

/// Where the port check stages what it hands the calls, in a [`Side`]'s
/// scratch region: the event `epoll_ctl` reads, the events a wait writes, a
/// deadline long past, a packet, and a byte to read into.
const EVENT_AT: u64 = SCRATCH + 0xA00;
const EVENTS_AT: u64 = SCRATCH + 0xA40;
const DEADLINE_AT: u64 = SCRATCH + 0xB00;
const PACKET_AT: u64 = SCRATCH + 0xB40;
const BYTE_AT: u64 = SCRATCH + 0xB80;

/// The cookie the port's descriptor is registered in the epoll set with.
const PORT_COOKIE: u64 = 0x4b34_706f_7274;

/// How long the kernel task waits before it queues the packet.
const QUEUE_AFTER_NANOS: u64 = 50_000_000;

/// How long the `epoll_wait` may take to be woken: under the one second a
/// wait on trusted queues sleeps between its own looks
/// (`fs::wake::TRUSTED_RECHECK_NANOS`), so only a wake ends it in time.
const WOKEN_WITHIN_NANOS: u64 = 900_000_000;

/// The port the kernel task queues a packet on.
static LATE_PORT: SpinLock<Option<Arc<Port>>> = SpinLock::new(None);

/// The kernel task: wait, then queue one user packet on [`LATE_PORT`].
fn queue_late(_argument: usize) {
    crate::sched::sleep_for(QUEUE_AFTER_NANOS);
    let port = LATE_PORT.lock().take();
    if let Some(port) = port {
        let _ = port.queue_user(PORT_COOKIE, [1, 2]);
    }
}

/// K4: a port as a descriptor an `epoll_wait` hears. The milliseconds the
/// woken wait took.
fn check_port_descriptor(report: &mut Report) -> Result<u64, &'static str> {
    let side = Side::new()?;
    let outcome = port_descriptor(&side, report);
    for fd in 3..16 {
        let _ = fd::sys_close(&side.process, fd);
    }
    side.close_everything();
    outcome
}

/// A descriptor a call answered.
fn descriptor(got: Result<usize, Errno>, what: &'static str) -> Result<i32, &'static str> {
    got.ok().and_then(|fd| i32::try_from(fd).ok()).ok_or(what)
}

/// `port_fd`'s refusals, on `port` in `side`: an unknown flag, a channel, a
/// handle without `WAIT`. The handle without `WAIT` is answered, to close.
fn port_fd_refusals(
    side: &Side,
    port: Handle,
    report: &mut Report,
) -> Result<Handle, &'static str> {
    let (channel, ..) = channel_in(&side.process, Rights::CHANNEL)?;
    let blind = side
        .process
        .with_handles(|table| {
            table.duplicate(
                port,
                ferrix_native_abi::rights::Requested::Exactly(Rights::READ),
            )
        })
        .map_err(|_| "could not make a port handle without WAIT")?;
    for (result, wanted, what) in [
        (
            side.call(nr::PORT_FD, &[reg(port), 2]),
            status::INVALID_ARGS,
            "port_fd with a flag it does not know was not refused with INVALID_ARGS",
        ),
        (
            side.call(nr::PORT_FD, &[reg(channel), 0]),
            status::WRONG_TYPE,
            "port_fd on a channel was not refused with WRONG_TYPE",
        ),
        (
            side.call(nr::PORT_FD, &[reg(blind), 0]),
            status::ACCESS_DENIED,
            "port_fd through a handle without WAIT was not refused with ACCESS_DENIED",
        ),
    ] {
        refused(result, wanted, what, report)?;
    }
    Ok(blind)
}

/// The body of [`check_port_descriptor`].
fn port_descriptor(side: &Side, report: &mut Report) -> Result<u64, &'static str> {
    let port = side.handle(nr::PORT_CREATE, &[], "port_create failed")?;
    let blind = port_fd_refusals(side, port, report)?;

    let process = &side.process;
    let watched = descriptor(
        side.call(nr::PORT_FD, &[reg(port), PORT_FD_CLOEXEC]),
        "port_fd on a port was refused",
    )?;
    if fd::sys_fcntl(process, watched, F_GETFD, 0) != Ok(FD_CLOEXEC as usize) {
        return Err("port_fd's PORT_FD_CLOEXEC did not reach the descriptor");
    }
    let file = fd::file(process, watched).map_err(|_| "port_fd's descriptor is not open")?;
    let the_port = fs_port(&file)?;
    if file.poll().readable {
        return Err("a port's descriptor polled readable with nothing queued");
    }
    let set = descriptor(
        syscall_check::call_by_number(process, Syscall::EpollCreate1, [0; 6]),
        "epoll_create1 was refused",
    )?;
    side.put(EVENT_AT, &epoll_calls::encode(EPOLLIN, PORT_COOKIE))?;
    let added = syscall_check::call_by_number(
        process,
        Syscall::EpollCtl,
        [
            set as u64,
            u64::from(EPOLL_CTL_ADD),
            watched as u64,
            EVENT_AT,
            0,
            0,
        ],
    );
    if added != Ok(0) {
        return Err("epoll_ctl refused a port's descriptor");
    }
    if !events(side, set, 0)?.is_empty() {
        return Err("epoll_wait reported a port's descriptor with nothing queued");
    }

    let took = check_a_packet_wakes_the_wait(side, set, &the_port)?;
    if !file.poll().readable {
        return Err("a port's descriptor did not poll readable with a packet queued");
    }
    if file::sys_read(process, watched, BYTE_AT, 1) != Err(Errno::EINVAL) {
        return Err("a read of a port's descriptor was not EINVAL");
    }

    side.put(DEADLINE_AT, &1_u64.to_ne_bytes())?;
    if side.call(nr::PORT_WAIT, &[reg(port), DEADLINE_AT, PACKET_AT]) != Ok(0) {
        return Err("port_wait did not take the packet a descriptor reported");
    }
    if file.poll().readable || !events(side, set, 0)?.is_empty() {
        return Err("a port's descriptor stayed readable once its packet was taken");
    }

    // The descriptor keeps the port once the handle has gone.
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(port)]);
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(blind)]);
    drop(the_port);
    let kept = fs_port(&file)?;
    if kept.queue_user(0, [0, 0]).is_err() || !file.poll().readable {
        return Err("a port's descriptor did not keep the port after its handle was closed");
    }
    Ok(took / 1_000_000)
}

/// A packet queued from another task, 50 ms into an `epoll_wait` of five
/// seconds on `set`, which watches `port`'s descriptor, ends the wait by a
/// wake and is reported. The nanoseconds the wait took.
fn check_a_packet_wakes_the_wait(
    side: &Side,
    set: i32,
    the_port: &Arc<Port>,
) -> Result<u64, &'static str> {
    let wakes_before = the_port.waiters().waits_ended_by_a_wake();
    *LATE_PORT.lock() = Some(Arc::clone(the_port));
    let task = crate::sched::spawn("port-fd check", queue_late, 0, ferrix_sched::NICE_0_WEIGHT)
        .map_err(|_| "no task to queue a packet from")?;
    let started = crate::timer::now_nanos();
    let reported = events(side, set, 5_000)?;
    let took = crate::timer::now_nanos().saturating_sub(started);
    crate::sched::wait_until_gone(&task, crate::sched::REAPER_PATIENCE_NANOS)?;
    let woken = the_port
        .waiters()
        .waits_ended_by_a_wake()
        .wrapping_sub(wakes_before);
    if reported != [(EPOLLIN, PORT_COOKIE)] {
        crate::console::println!(
            "  initcall epoll_wait on a port's descriptor reported {} events after {} ms",
            reported.len(),
            took / 1_000_000
        );
        return Err("epoll_wait on a port's descriptor did not report EPOLLIN with its cookie");
    }
    if woken == 0 || took >= WOKEN_WITHIN_NANOS {
        crate::console::println!(
            "  initcall epoll_wait on a port's descriptor ended after {} ms, {woken} waits woken",
            took / 1_000_000
        );
        return Err("a packet queued on a port did not wake an epoll_wait on its descriptor");
    }
    Ok(took)
}

/// The port behind a `port_fd` descriptor.
fn fs_port(file: &ferrix_vfs::OpenFile) -> Result<Arc<Port>, &'static str> {
    crate::fs::portfd::of(file)
        .map(|file| Arc::clone(file.port()))
        .ok_or("port_fd's descriptor is not a port's")
}

/// `epoll_wait` on `set` for up to four events and `timeout` milliseconds:
/// each event's mask and cookie.
fn events(side: &Side, set: i32, timeout: i32) -> Result<Vec<(u32, u64)>, &'static str> {
    let count = epoll_calls::sys_epoll_wait(&side.process, set, EVENTS_AT, 4, timeout)
        .map_err(|_| "epoll_wait was refused")?;
    let bytes = side.get(EVENTS_AT, count * EVENT_BYTES)?;
    Ok(bytes
        .chunks_exact(EVENT_BYTES)
        .map(|event| {
            let mut mask = [0_u8; 4];
            let mut cookie = [0_u8; 8];
            for (slot, byte) in mask.iter_mut().zip(event) {
                *slot = *byte;
            }
            for (slot, byte) in cookie.iter_mut().zip(event.iter().skip(EVENT_BYTES - 8)) {
                *slot = *byte;
            }
            (u32::from_le_bytes(mask), u64::from_le_bytes(cookie))
        })
        .collect())
}

/// Make native call `number` as `caller`.
fn call(caller: &Arc<Process>, number: usize, args: &[u64]) -> Result<usize, Errno> {
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
    native::dispatch(&args, Some(&**caller))
}

/// A handle as a register.
fn reg(handle: Handle) -> u64 {
    u64::from(handle.0)
}

/// A channel, one end in `holder`'s table with `rights` and the other kept.
fn channel_in(
    holder: &Process,
    rights: Rights,
) -> Result<(Handle, Arc<Endpoint>, Arc<Endpoint>), &'static str> {
    let (near, far) = Endpoint::pair().map_err(|_| "no memory for a channel")?;
    let handle = holder
        .with_handles(|table| table.insert(Object::Channel(Arc::clone(&near)), rights))
        .map_err(|_| "no room for a channel end")?;
    Ok((handle, near, far))
}

/// Whether `handle` in `holder` names the channel end `end`, with `rights`.
fn names(holder: &Process, handle: Handle, end: &Arc<Endpoint>, rights: Rights) -> bool {
    holder.with_handles(|table| {
        matches!(
            table.get(handle),
            Ok((Object::Channel(named), held)) if Arc::ptr_eq(named, end) && held == rights
        )
    })
}

/// Require `result` to be exactly the refusal `wanted`.
fn refused(
    result: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    if result == Err(wanted) {
        report.refusals += 1;
        Ok(())
    } else {
        Err(what)
    }
}

/// Close every handle in each of `processes`, as their ends would.
fn close_all(processes: &[&Arc<Process>]) {
    for process in processes {
        object::dispose(process.with_handles(object::HandleTable::clear));
    }
}

/// The calls, by number, between a process and forks of it.
fn check_give_and_take(report: &mut Report) -> Result<(), &'static str> {
    let parent = process::new_for_check().map_err(|_| "could not make a parent")?;
    let stranger = process::new_for_check().map_err(|_| "could not make a stranger")?;
    let child = process::fork_for_check(&parent).map_err(|_| "could not fork a child")?;
    let outcome = give_and_take(&parent, &stranger, &child, report);
    close_all(&[&parent, &stranger, &child]);
    outcome
}

/// The body of [`check_give_and_take`].
///
/// Verifies: L.object.89
fn give_and_take(
    parent: &Arc<Process>,
    stranger: &Arc<Process>,
    child: &Arc<Process>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let pid = u64::from(child.pid());
    let (given, near, far) = channel_in(parent, Rights::CHANNEL)?;
    let (strangers, ..) = channel_in(stranger, Rights::CHANNEL)?;

    refused(
        call(stranger, nr::PROCESS_GIVE, &[pid, reg(strangers)]),
        status::NOT_CHILD,
        "process_give into another's child was not refused with NOT_CHILD",
        report,
    )?;
    refused(
        call(parent, nr::PROCESS_GIVE, &[0, reg(given)]),
        status::NO_PROCESS,
        "process_give to pid 0 was not refused with NO_PROCESS",
        report,
    )?;
    refused(
        call(parent, nr::PROCESS_GIVE, &[1 << 40, reg(given)]),
        status::NO_PROCESS,
        "process_give to a pid wider than 32 bits was not refused with NO_PROCESS",
        report,
    )?;
    let kept = parent
        .with_handles(|table| {
            table.duplicate(
                given,
                ferrix_native_abi::rights::Requested::Exactly(Rights::READ),
            )
        })
        .map_err(|_| "could not make a handle without TRANSFER")?;
    refused(
        call(parent, nr::PROCESS_GIVE, &[pid, reg(kept)]),
        status::ACCESS_DENIED,
        "process_give of a handle without TRANSFER was not refused with ACCESS_DENIED",
        report,
    )?;
    refused(
        call(parent, nr::PROCESS_GIVE, &[pid, 0]),
        status::BAD_HANDLE,
        "process_give of handle zero was not refused with BAD_HANDLE",
        report,
    )?;
    if !names(parent, given, &near, Rights::CHANNEL) || !names(parent, kept, &near, Rights::READ) {
        return Err("a refused process_give did not leave the handle with the caller");
    }
    if call(child, nr::PROCESS_BOOTSTRAP, &[]) != Ok(0) {
        return Err("process_bootstrap in a process given nothing did not answer zero");
    }

    if call(parent, nr::PROCESS_GIVE, &[pid, reg(given)]) != Ok(0) {
        return Err("process_give of a channel end to the caller's child was refused");
    }
    if parent.with_handles(|table| table.get(given).is_ok()) {
        return Err("process_give left the handle in the caller's table: a copy, not a move");
    }
    let (second, ..) = channel_in(parent, Rights::CHANNEL)?;
    refused(
        call(parent, nr::PROCESS_GIVE, &[pid, reg(second)]),
        status::ALREADY_BOUND,
        "a second process_give to one child was not refused with ALREADY_BOUND",
        report,
    )?;
    let taken = call(child, nr::PROCESS_BOOTSTRAP, &[])
        .map_err(|_| "process_bootstrap in a child given a handle was refused")?;
    let taken = Handle(u32::try_from(taken).map_err(|_| "process_bootstrap answered no handle")?);
    if !taken.is_valid() || !names(child, taken, &near, Rights::CHANNEL) {
        return Err("process_bootstrap did not answer the end given, with the rights it had");
    }
    if call(child, nr::PROCESS_BOOTSTRAP, &[]) != Ok(0) {
        return Err("a second process_bootstrap did not answer zero");
    }
    refused(
        call(parent, nr::PROCESS_GIVE, &[pid, reg(second)]),
        status::ALREADY_BOUND,
        "process_give after the child took its bootstrap was not refused with ALREADY_BOUND",
        report,
    )?;
    report.taken += 1;
    drop(far);

    check_execve_seals_and_keeps(parent, second, report)?;
    check_an_end_closes_it(parent, report)
}

/// An `execve` with nothing given refuses a give from then on; one given
/// before an `execve` is still there after it.
///
/// Verifies: L.object.91
fn check_execve_seals_and_keeps(
    parent: &Arc<Process>,
    spare: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let sealed = process::fork_for_check(parent).map_err(|_| "could not fork a child")?;
    let kept = process::fork_for_check(parent).map_err(|_| "could not fork a child")?;
    let outcome = (|| {
        sealed.mark_execed();
        refused(
            call(
                parent,
                nr::PROCESS_GIVE,
                &[u64::from(sealed.pid()), reg(spare)],
            ),
            status::BAD_STATE,
            "process_give to a child that had completed an execve was not refused with BAD_STATE",
            report,
        )?;
        if !parent.with_handles(|table| table.get(spare).is_ok()) {
            return Err("a process_give refused after an execve did not leave the handle");
        }
        let (given, near, _far) = channel_in(parent, Rights::CHANNEL)?;
        if call(
            parent,
            nr::PROCESS_GIVE,
            &[u64::from(kept.pid()), reg(given)],
        ) != Ok(0)
        {
            return Err("process_give to a child that had not yet exec'd was refused");
        }
        kept.mark_execed();
        let taken = call(&kept, nr::PROCESS_BOOTSTRAP, &[])
            .ok()
            .and_then(|value| u32::try_from(value).ok())
            .map(Handle)
            .ok_or("process_bootstrap after an execve was refused")?;
        if !names(&kept, taken, &near, Rights::CHANNEL) {
            return Err("a bootstrap given before an execve was not there after it");
        }
        report.taken += 1;
        Ok(())
    })();
    close_all(&[&sealed, &kept]);
    outcome
}

/// A bootstrap never taken is closed as its holder ends, and the peer end
/// hears it; an ended child refuses a give.
///
/// Verifies: L.object.90
fn check_an_end_closes_it(parent: &Arc<Process>, report: &mut Report) -> Result<(), &'static str> {
    let doomed = process::fork_for_check(parent).map_err(|_| "could not fork a child")?;
    let (given, near, far) = channel_in(parent, Rights::CHANNEL)?;
    if call(
        parent,
        nr::PROCESS_GIVE,
        &[u64::from(doomed.pid()), reg(given)],
    ) != Ok(0)
    {
        return Err("process_give to a child about to end was refused");
    }
    // Held here and in the slot: the slot's end must go for the peer to hear.
    drop(near);
    process::kill(&doomed, object::job::KILLED_STATUS);
    if !far.signals().intersects(Signals::PEER_CLOSED) {
        return Err("a bootstrap never taken was not closed when its holder ended");
    }
    let (spare, ..) = channel_in(parent, Rights::CHANNEL)?;
    refused(
        call(
            parent,
            nr::PROCESS_GIVE,
            &[u64::from(doomed.pid()), reg(spare)],
        ),
        status::BAD_STATE,
        "process_give to a child that had ended was not refused with BAD_STATE",
        report,
    )
}

/// A fork given a channel end runs a program that `execve`s another, which
/// takes its bootstrap by number and closes it; its parent, given nothing,
/// runs the same and is answered zero. `false` with no program to run.
fn check_a_program_takes_it_after_execve() -> Result<bool, &'static str> {
    if arch::USER_BOOTSTRAP_PROGRAM.is_empty() || arch::USER_EXEC_PROGRAM.is_empty() {
        return Ok(false);
    }
    let class = if size_of::<usize>() == 8 {
        ferrix_elf::Class::Elf64
    } else {
        ferrix_elf::Class::Elf32
    };
    let machine = arch::ARCH.elf_machine();
    let target = image::build_with(
        class,
        machine,
        image::Shape::Good,
        arch::USER_BOOTSTRAP_PROGRAM,
    );
    let mut code = arch::USER_EXEC_PROGRAM.to_vec();
    let path_at = code
        .len()
        .checked_sub(EXEC_PATH.len())
        .filter(|&at| code.get(at..) == Some(EXEC_PATH))
        .ok_or("the program that calls execve does not end with the path it runs")?;
    code.truncate(path_at);
    code.extend_from_slice(EXEC_TARGET);
    let caller = image::build_with(class, machine, image::Shape::Good, &code);

    // Where the child's `execve` will look: the btrfs root once
    // `root_disk::switch` has put it in place, whose `/tmp` is its own
    // tmpfs, not the kernel's. Written through the kernel's namespace, the
    // program would be where no new process can see it.
    let ns = crate::fs::namespace();
    let ctx = crate::fs::root_disk::process_context();
    let create = OpenFlags {
        write: true,
        create: true,
        truncate: true,
        ..OpenFlags::default()
    };
    let path = EXEC_TARGET.strip_suffix(b"\0").unwrap_or(EXEC_TARGET);
    let file = ns
        .open(&ctx, None, path, &create, 0o755)
        .map_err(|_| "could not create the program that takes its bootstrap")?;
    let written = file.write(&target);
    drop(file);
    let outcome = if written == Ok(target.len()) {
        run_the_programs(&caller)
    } else {
        Err("could not write the program that takes its bootstrap")
    };
    let _ = ns.unlink(&ctx, None, path);
    outcome.map(|()| true)
}

/// The body of [`check_a_program_takes_it_after_execve`], with the program
/// at [`EXEC_TARGET`].
fn run_the_programs(caller: &[u8]) -> Result<(), &'static str> {
    let parent = exec::load(
        caller,
        &[b"/bootstrap-caller"],
        &[],
        [0x4b; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that calls execve could not be loaded")?;
    let child = process::fork_for_check(&parent)
        .map_err(|_| "a program that calls execve could not be forked")?;
    let (given, near, far) = channel_in(&parent, Rights::CHANNEL)?;
    drop(near);
    if call(
        &parent,
        nr::PROCESS_GIVE,
        &[u64::from(child.pid()), reg(given)],
    ) != Ok(0)
    {
        return Err("process_give to a fork about to execve was refused");
    }
    let status = run_to_its_end(&child)?;
    if status != 0 {
        crate::console::println!(
            "  initcall a program given a bootstrap before its execve exited with {status}"
        );
        return Err("a program given a bootstrap before its execve did not take and close it");
    }
    if !far.signals().intersects(Signals::PEER_CLOSED) {
        return Err("the handle a program took with process_bootstrap was not the end given");
    }
    let status = run_to_its_end(&parent)?;
    if status != NOTHING_STATUS {
        crate::console::println!(
            "  initcall a program given no bootstrap exited with {status}, not {NOTHING_STATUS}"
        );
        return Err("process_bootstrap in a program given nothing did not answer zero");
    }
    Ok(())
}

/// Start `process`, and wait for it to end and its task to be gone.
fn run_to_its_end(process: &Arc<Process>) -> Result<i32, &'static str> {
    let task = process::start(process).map_err(|_| "a program the check made would not start")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let status = process
        .wait_for_exit(deadline)
        .ok_or("a program the check made never ended")?;
    crate::sched::wait_until_gone(&task, crate::sched::REAPER_PATIENCE_NANOS)?;
    Ok(status)
}
