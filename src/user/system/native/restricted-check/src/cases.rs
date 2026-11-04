//! The cases (`docs/microkernel/K1.md` §10, `docs/microkernel/K11.md` §8). A
//! failing case's number is the program's exit status.

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::nr;
use ferrix_native_abi::restricted::{Context, FaultKind, HANDLE_KEEP, Reason, SHARED_BASE};
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::status;
use ferrix_rt::linux;
use ferrix_rt::restricted::{
    Exit, continue_, copy_from_program, declare_copy_window, enter, resume,
};

use crate::arch;

/// Where the images are mapped in the restricted space: under 4 GiB, so that
/// the 32-bit one can run too.
const IMAGE_AT: u64 = 0x1000_0000;
/// The restricted stack.
const STACK_AT: u64 = 0x1100_0000;
/// One past the restricted stack's top.
const STACK_TOP: u64 = STACK_AT + 0x4000;
/// A demand-zero page of the restricted space.
const ZERO_AT: u64 = 0x1200_0000;
/// A read-only page of the restricted space.
const READ_ONLY_AT: u64 = 0x1300_0000;
/// An address nothing is mapped at, in the restricted window.
const HOLE: u64 = 0x2000_0000;

/// What the kernel's check asks a message's first byte to mean: sweep.
const ASK_SWEEP: u8 = 1;
/// Read the barrier count.
const ASK_COUNT_START: u8 = 2;
/// Compare the barrier count: its delta must be the word that follows.
const ASK_COUNT_END: u8 = 3;
/// Pin the thread whose handle comes with this to the processor the word
/// names, modulo how many there are.
const ASK_PIN: u8 = 4;
/// Whether the thread whose handle comes with it runs on processor `word` now.
const ASK_ON_PROCESSOR: u8 = 12;
/// Whether there are two processors to read a shootdown across.
const ASK_TWO_PROCESSORS: u8 = 13;
/// `ASK_PIN`'s word for no pin.
const ANY_PROCESSOR: u64 = u64::MAX;
/// The barrier count must have moved since [`ASK_COUNT_START`].
const ASK_COUNT_MOVED: u8 = 5;
/// Nothing: a blocking round trip.
const ASK_NOTHING: u8 = 6;
/// The counter's reading now, for the check to scale the row by.
const ASK_ROW_START: u8 = 8;
/// The counter's ticks since [`ASK_ROW_START`].
const ASK_ROW_TICKS: u8 = 9;
/// The row's median of p50s, in ticks.
const ASK_ROW_P50: u8 = 10;
/// In the first message: spin until killed.
const ASK_WAIT_TO_BE_KILLED: u8 = 7;

/// A native call with no memory the wrapper names: the status decoded.
fn sys(number: usize, args: [u64; 6]) -> Result<u64, Errno> {
    // SAFETY: each caller's pointer arguments are its own, valid for the call.
    unsafe {
        linux::call(
            number,
            [
                args[0] as usize,
                args[1] as usize,
                args[2] as usize,
                args[3] as usize,
                args[4] as usize,
                args[5] as usize,
            ],
        )
    }
    .map(|value| value as u64)
}

/// A case's verdict: its number on failure.
fn hold(condition: bool, case: i32) -> Result<(), i32> {
    if condition { Ok(()) } else { Err(case) }
}

/// What the case asked of `result`: refused with `want`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "each case hands over what its call answered"
)]
fn refused<T>(result: Result<T, Errno>, want: Errno, case: i32) -> Result<(), i32> {
    match result {
        Err(got) if got == want => Ok(()),
        _ => Err(case),
    }
}

/// The handles the kernel's check gives this program.
struct Given {
    /// The channel it talks to the check on.
    channel: u64,
    /// The region.
    region: u64,
    /// Its job.
    job: u64,
    /// The restricted space.
    space: u64,
    /// The region's space.
    region_space: u64,
}

/// Ask the check something, and wait for its yes.
fn asked(given: &Given, what: u8, word: u64, handle: Option<u64>) -> Result<bool, i32> {
    let mut message = [0_u8; 9];
    message[0] = what;
    message[1..].copy_from_slice(&word.to_ne_bytes());
    let handles = handle.map_or([0_u32; 1], |value| [value as u32]);
    let count = u64::from(handle.is_some());
    sys(
        nr::CHANNEL_WRITE,
        [
            given.channel,
            message.as_ptr().addr() as u64,
            9,
            handles.as_ptr().addr() as u64,
            count,
            0,
        ],
    )
    .map_err(|_| 90)?;
    let mut observed = 0_u32;
    let _ = sys(
        nr::OBJECT_WAIT_ONE,
        [
            given.channel,
            u64::from(ferrix_native_abi::signals::Signals::READABLE.0),
            0,
            (&raw mut observed).addr() as u64,
            0,
            0,
        ],
    )
    .map_err(|_| 91)?;
    let mut answer = [0_u8; 1];
    let mut sizes = [0_u64; 2];
    sys(
        nr::CHANNEL_READ,
        [
            given.channel,
            answer.as_mut_ptr().addr() as u64,
            1,
            0,
            0,
            sizes.as_mut_ptr().addr() as u64,
        ],
    )
    .map_err(|_| 92)?;
    Ok(answer[0] == 1)
}

/// [`asked`], where the only good answer is yes.
fn ask(given: &Given, what: u8, word: u64, handle: Option<u64>) -> Result<(), i32> {
    hold(asked(given, what, word, handle)?, 93)
}

/// Run a system call's exit, and require `SYSCALL`.
fn syscall_exit(exit: &Exit, number: u64, case: i32) -> Result<(), i32> {
    hold(
        exit.reason == Reason::Syscall as u64 && exit.number == number,
        case,
    )
}

/// Require a `FAULT` exit with this vector.
fn fault_exit(exit: &Exit, vector: u64, case: i32) -> Result<(), i32> {
    hold(
        exit.reason == Reason::Fault as u64 && exit.number == vector,
        case,
    )
}

/// The context the last exit left, as a read gives it.
fn read() -> Result<Context, i32> {
    ferrix_rt::native::region::context_read(ferrix_rt::Kernel).map_err(|_| 94)
}

/// Run every case.
pub(crate) fn run(bootstrap: ferrix_rt::Bootstrap) -> i32 {
    // Started with nothing, it is the check's process that faults in normal mode
    // (K11 §5): its region's job ends, and this load of a hole does not return.
    if bootstrap.is_none() {
        // SAFETY: none: the load is the point, and faults.
        let _ = unsafe { core::ptr::read_volatile(HOLE as *const u64) };
        return 140;
    }
    match cases(bootstrap) {
        Ok(()) => 0,
        Err(case) => case,
    }
}

/// Take the handles the check sent.
fn receive(bootstrap: ferrix_rt::Bootstrap) -> Result<Given, i32> {
    let channel = bootstrap.ok_or(95)?;
    let channel = u64::from(channel.into_owned().into_raw().0);
    let mut handles = [0_u32; 4];
    let mut byte = [0_u8; 1];
    let mut sizes = [0_u64; 2];
    sys(
        nr::CHANNEL_READ,
        [
            channel,
            byte.as_mut_ptr().addr() as u64,
            1,
            handles.as_mut_ptr().addr() as u64,
            4,
            sizes.as_mut_ptr().addr() as u64,
        ],
    )
    .map_err(|_| 96)?;
    // Told to wait: the region's other process, which a fault there must end.
    if byte[0] == ASK_WAIT_TO_BE_KILLED {
        loop {
            core::hint::spin_loop();
        }
    }
    Ok(Given {
        channel,
        region: u64::from(handles[0]),
        job: u64::from(handles[1]),
        space: u64::from(handles[2]),
        region_space: u64::from(handles[3]),
    })
}

/// The restricted space's memory: the images, a stack, the rest demand-zero or
/// read-only.
fn map_space(space: u64) -> Result<(), i32> {
    let code = arch::image();
    let length = (code.len() as u64 + 0xFFF) & !0xFFF;
    let vmo = sys(nr::VMO_CREATE, [length, 0, 0, 0, 0, 0]).map_err(|_| 10)?;
    let offset = 0_u64;
    sys(
        nr::VMO_WRITE,
        [
            vmo,
            code.as_ptr().addr() as u64,
            code.len() as u64,
            (&raw const offset).addr() as u64,
            0,
            0,
        ],
    )
    .map_err(|_| 11)?;
    let vmo = sys(nr::VMO_EXECUTABLE, [vmo, 0, 0, 0, 0, 0]).map_err(|_| 12)?;
    let protection =
        u64::from(ferrix_native_abi::types::MAP_READ | ferrix_native_abi::types::MAP_EXECUTE);
    sys(
        nr::SPACE_MAP,
        [
            space,
            vmo,
            IMAGE_AT,
            length,
            protection,
            (&raw const offset).addr() as u64,
        ],
    )
    .map_err(|_| 13)?;
    let write = u64::from(ferrix_native_abi::types::MAP_READ | ferrix_native_abi::types::MAP_WRITE);
    sys(nr::SPACE_ALLOCATE, [space, STACK_AT, 0x4000, write, 0, 0]).map_err(|_| 14)?;
    sys(nr::SPACE_ALLOCATE, [space, ZERO_AT, 0x1000, write, 0, 0]).map_err(|_| 15)?;
    let read = u64::from(ferrix_native_abi::types::MAP_READ);
    sys(
        nr::SPACE_ALLOCATE,
        [space, READ_ONLY_AT, 0x1000, read, 0, 0],
    )
    .map_err(|_| 16)?;
    Ok(())
}

/// A context at an image's offset, on the restricted stack.
fn at(offset: u64) -> Context {
    arch::context(IMAGE_AT + offset, STACK_TOP)
}

/// Every case, in order.
fn cases(bootstrap: ferrix_rt::Bootstrap) -> Result<(), i32> {
    let given = receive(bootstrap)?;
    map_space(given.space)?;
    round_trip()?;
    faults()?;
    shared_region(&given)?;
    forged()?;
    native_from_restricted()?;
    all_calls_exit()?;
    continue_after_fault()?;
    tables(&given)?;
    moves()?;
    copy_window()?;
    vectors()?;
    if arch::HAS_COMPAT {
        compat()?;
    }
    barrier(&given)?;
    pointers()?;
    kicks()?;
    preempted(&given)?;
    interrupted_wait()?;
    interrupted_round_trip()?;
    second_process(&given)?;
    kills(&given)?;
    row(&given)?;
    shared_starts_shared(&given)?;
    // Last, as it leaves this thread pinned, and case 132 reads nothing with the two
    // on one processor: its control passed with this case ahead of it.
    restricted_shootdown(&given)?;
    Ok(())
}

/// Case 20: a Linux number with six marked arguments exits with them, and
/// `RESTRICTED_RESUME` gives the value back with everything else as it was.
fn round_trip() -> Result<(), i32> {
    let mut context = at(arch::sys());
    arch::mark_every_register(&mut context);
    let words = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    arch::set_call(&mut context, arch::GETPPID, words);
    let exit = enter(&context);
    syscall_exit(&exit, arch::GETPPID, 20)?;
    hold(exit.words == words, 21)?;
    let exit = resume(0x77);
    fault_exit(&exit, arch::VECTOR_BREAKPOINT, 22)?;
    let back = read()?;
    hold(arch::result_of(&back) == 0x77, 23)?;
    hold(arch::every_register_marked(&back), 24)?;
    hold(arch::pc_of(&back) == exit.words[3], 25)?;
    // `RESTRICTED_RESUME` after a fault is refused, and leaves the reason zero.
    let refused_resume = resume(1);
    hold(refused_resume.reason == 0, 26)?;
    hold(
        refused_resume.number as i64 == status::BAD_STATE.as_return_value() as i64,
        27,
    )?;
    Ok(())
}

/// Cases 30 to 37: each fault exits with its vector and kind; demand-zero does
/// not exit.
fn faults() -> Result<(), i32> {
    let mut load = at(arch::load());
    arch::set_first(&mut load, HOLE);
    let exit = enter(&load);
    fault_exit(&exit, arch::VECTOR_PAGE, 30)?;
    hold(
        exit.words[1] == HOLE && exit.words[2] == FaultKind::NoMapping as u64,
        31,
    )?;
    let mut store = at(arch::store());
    arch::set_first(&mut store, READ_ONLY_AT);
    let exit = enter(&store);
    fault_exit(&exit, arch::VECTOR_PAGE, 32)?;
    hold(exit.words[2] == FaultKind::Protection as u64, 33)?;
    let mut fetch = at(arch::fetch());
    arch::set_first(&mut fetch, ZERO_AT);
    let exit = enter(&fetch);
    fault_exit(&exit, arch::VECTOR_FETCH, 34)?;
    let exit = enter(&at(arch::undefined()));
    fault_exit(&exit, arch::VECTOR_UNDEFINED, 35)?;
    hold(exit.words[2] == FaultKind::NotMemory as u64, 35)?;
    if arch::HAS_DIVIDE {
        let exit = enter(&at(arch::divide()));
        fault_exit(&exit, arch::VECTOR_DIVIDE, 36)?;
    }
    let mut touch = at(arch::touch());
    arch::set_first(&mut touch, ZERO_AT);
    let exit = enter(&touch);
    fault_exit(&exit, arch::VECTOR_BREAKPOINT, 37)?;
    hold(arch::result_of(&read()?) == 7, 37)?;
    Ok(())
}

/// Cases 40 to 46: nothing of the region reachable from restricted mode: load,
/// store and fetch at this program's own code and data, and the kernel's sweep.
#[allow(clippy::too_many_lines, reason = "one case after another, in order")]
fn shared_region(given: &Given) -> Result<(), i32> {
    let code = (run as *const ()).addr() as u64;
    let data = (&raw const PRIVATE_PORT).addr() as u64;
    hold(code >= SHARED_BASE, 40)?;
    let mut load = at(arch::load());
    arch::set_first(&mut load, code);
    let exit = enter(&load);
    fault_exit(&exit, arch::VECTOR_PAGE, 41)?;
    hold(exit.words[2] == FaultKind::NoMapping as u64, 41)?;
    let mut store = at(arch::store());
    arch::set_first(&mut store, data);
    let exit = enter(&store);
    fault_exit(&exit, arch::VECTOR_PAGE, 42)?;
    hold(exit.words[2] == FaultKind::NoMapping as u64, 42)?;
    let mut fetch = at(arch::fetch());
    arch::set_first(&mut fetch, code);
    let exit = enter(&fetch);
    fault_exit(&exit, arch::VECTOR_FETCH, 43)?;
    hold(exit.words[2] == FaultKind::NoMapping as u64, 43)?;
    // A root slot of its own, entered from normal mode so that the normal root
    // copies it, and emptied: its top table must stay (`L.user.180`).
    const OTHER_SLOT: u64 = 0x0100_0000_0000;
    let write = u64::from(ferrix_native_abi::types::MAP_READ | ferrix_native_abi::types::MAP_WRITE);
    sys(
        nr::SPACE_ALLOCATE,
        [given.space, OTHER_SLOT, 0x1000, write, 0, 0],
    )
    .map_err(|_| 44)?;
    let mut touch = at(arch::touch());
    arch::set_first(&mut touch, OTHER_SLOT);
    fault_exit(&enter(&touch), arch::VECTOR_BREAKPOINT, 44)?;
    declare_copy_window().map_err(|_| 44)?;
    hold(copy_word(OTHER_SLOT) == Ok(7), 44)?;
    // By handle a space is that space: its own handle never reaches the region
    // (L6).
    let mut out = [0_u8; 8];
    refused(
        sys(
            nr::SPACE_READ,
            [given.space, code, out.as_mut_ptr().addr() as u64, 8, 0, 0],
        ),
        status::FAULT,
        45,
    )?;
    // A space bound already, offered again by a duplicate of its handle, is
    // refused, and the handle stays (B3).
    let name = b"twice";
    let again = sys(
        nr::HANDLE_DUPLICATE,
        [given.space, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    )
    .map_err(|_| 48)?;
    refused(
        sys(
            nr::PROCESS_CREATE_SHARED,
            [
                given.region,
                given.job,
                again,
                name.as_ptr().addr() as u64,
                name.len() as u64,
                0,
            ],
        ),
        status::BAD_STATE,
        48,
    )?;
    sys(nr::HANDLE_CLOSE, [again, 0, 0, 0, 0, 0]).map_err(|_| 48)?;
    // And a space bound to a process not started yet, whose space nothing has claimed:
    // one bind wins (B3).
    let fresh = sys(nr::SPACE_CREATE, [0; 6]).map_err(|_| 139)?;
    let again = sys(
        nr::HANDLE_DUPLICATE,
        [fresh, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    )
    .map_err(|_| 139)?;
    let mut bound = 0_u64;
    let first = sys(
        nr::PROCESS_CREATE_SHARED,
        [
            given.region,
            given.job,
            fresh,
            name.as_ptr().addr() as u64,
            name.len() as u64,
            (&raw mut bound).addr() as u64,
        ],
    )
    .map_err(|_| 139)?;
    refused(
        sys(
            nr::PROCESS_CREATE_SHARED,
            [
                given.region,
                given.job,
                again,
                name.as_ptr().addr() as u64,
                name.len() as u64,
                0,
            ],
        ),
        status::BAD_STATE,
        139,
    )?;
    sys(nr::HANDLE_CLOSE, [again, 0, 0, 0, 0, 0]).map_err(|_| 139)?;
    sys(nr::HANDLE_CLOSE, [first, 0, 0, 0, 0, 0]).map_err(|_| 139)?;
    // A full table: the thread's handle has no number, so no thread ran (B8).
    full_table_runs_no_thread()?;
    // A thread's entry below the region (B8).
    refused(
        sys(
            nr::THREAD_CREATE,
            [IMAGE_AT, top(&KICKED_STACK), 0, 0, 0, 0],
        ),
        status::INVALID_ARGS,
        46,
    )?;
    sys(nr::SPACE_UNMAP, [given.space, OTHER_SLOT, 0x1000, 0, 0, 0]).map_err(|_| 44)?;
    ask(given, ASK_SWEEP, 0, None)
}

/// Case 50: every forged context refused, held state unchanged, and a good one
/// still works. A number above it names the forgery that went through.
fn forged() -> Result<(), i32> {
    let good = at(arch::sys());
    let mut case = 0;
    while let Some(bad) = arch::forged(&good, case) {
        let exit = enter(&bad);
        if exit.reason != 0 || exit.number as i64 != status::INVALID_ARGS.as_return_value() as i64 {
            return Err(100 + case as i32);
        }
        case += 1;
    }
    let mut context = good;
    arch::set_call(&mut context, arch::GETPPID, [0; 6]);
    syscall_exit(&enter(&context), arch::GETPPID, 50)?;
    fault_exit(&resume(1), arch::VECTOR_BREAKPOINT, 51)
}

/// Cases 52 to 57: a native number from restricted mode.
fn native_from_restricted() -> Result<(), i32> {
    // Answered in place, against the private table: nothing is there.
    let mut context = at(arch::sys());
    arch::set_call(
        &mut context,
        nr::HANDLE_DUPLICATE as u64,
        [1, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    );
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 52)?;
    hold(
        arch::result_of(&read()?) as i64 == status::BAD_HANDLE.as_return_value() as i64,
        52,
    )?;
    // A normal-mode call: refused, and still in restricted mode.
    let mut context = at(arch::sys());
    arch::set_call(&mut context, nr::RESTRICTED_ENTER as u64, [0; 6]);
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 53)?;
    hold(
        arch::result_of(&read()?) as i64 == status::BAD_STATE.as_return_value() as i64,
        53,
    )?;
    // So is one that would run code in the process (I4).
    let mut context = at(arch::sys());
    arch::set_call(&mut context, nr::THREAD_CREATE as u64, [0; 6]);
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 47)?;
    hold(
        arch::result_of(&read()?) as i64 == status::BAD_STATE.as_return_value() as i64,
        47,
    )?;
    // And `devmgr`'s start, which no program's call may ask for (B6).
    let mut context = at(arch::sys());
    arch::set_call(&mut context, nr::DEVMGR_START as u64, [0; 6]);
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 49)?;
    hold(
        arch::result_of(&read()?) as i64 == status::BAD_STATE.as_return_value() as i64,
        49,
    )?;
    // `PORT_FD` exits, as a Linux number does.
    let mut context = at(arch::sys());
    arch::set_call(&mut context, nr::PORT_FD as u64, [7, 0, 0, 0, 0, 0]);
    let exit = enter(&context);
    syscall_exit(&exit, nr::PORT_FD as u64, 54)?;
    hold(exit.words[0] == 7, 54)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 54)?;
    // A number with high bits set is a Linux number.
    let wild = 0x1000_0000_0000_1000_u64;
    let mut context = at(arch::sys());
    arch::set_call(&mut context, wild, [0; 6]);
    syscall_exit(&enter(&context), wild, 55)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 55)?;
    Ok(())
}

/// Case 153: a process of a region is not started by `PROCESS_START` or `PROCESS_RUN`, from
/// either mode (ledger 6575 D3, 6649).
fn shared_starts_shared(given: &Given) -> Result<(), i32> {
    let name = b"plain";
    let mut bound = 0_u64;
    let shared = sys(
        nr::PROCESS_CREATE_SHARED,
        [
            given.region,
            given.job,
            0,
            name.as_ptr().addr() as u64,
            name.len() as u64,
            (&raw mut bound).addr() as u64,
        ],
    )
    .map_err(|_| 153)?;
    // Every answer is gathered before the handles close, so that a refusal that
    // does not hold reads as this case and not as what an open handle leaves.
    let normal_start = refused(
        sys(nr::PROCESS_START, [shared, 0, 0, 0, 0, 0]),
        status::BAD_STATE,
        153,
    );
    let normal_run = refused(
        sys(nr::PROCESS_RUN, [shared, IMAGE_AT, STACK_TOP, 0, 0, 0]),
        status::BAD_STATE,
        153,
    );
    // The move takes `shared` out of the shared table: only the private handle is
    // left to close, from the mode whose table it is in.
    let Ok(private) = sys(
        nr::HANDLE_TO_PRIVATE,
        [shared, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    ) else {
        let _ = sys(nr::HANDLE_CLOSE, [shared, 0, 0, 0, 0, 0]);
        return Err(153);
    };
    let restricted_start = refused_in_restricted(nr::PROCESS_START, [private, 0, 0, 0, 0, 0]);
    let restricted_run =
        refused_in_restricted(nr::PROCESS_RUN, [private, IMAGE_AT, STACK_TOP, 0, 0, 0]);
    let mut close = at(arch::sys());
    arch::set_call(
        &mut close,
        nr::HANDLE_CLOSE as u64,
        [private, 0, 0, 0, 0, 0],
    );
    let closed = fault_exit(&enter(&close), arch::VECTOR_BREAKPOINT, 153);
    normal_start
        .and(normal_run)
        .and(restricted_start)
        .and(restricted_run)
        .and(closed)
}

/// Case 153's call from restricted mode: `number`, refused `BAD_STATE`.
fn refused_in_restricted(number: usize, args: [u64; 6]) -> Result<(), i32> {
    let mut context = at(arch::sys());
    arch::set_call(&mut context, number as u64, args);
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 153)?;
    hold(
        arch::result_of(&read()?) as i64 == status::BAD_STATE.as_return_value() as i64,
        153,
    )
}

/// Cases 58, 59: `RESTRICTED_CONTINUE` retries a fault as it stands, and what a
/// program's `popf` left is read normalised, so that an entry takes it.
fn continue_after_fault() -> Result<(), i32> {
    let mut load = at(arch::load());
    arch::set_first(&mut load, ZERO_AT + 8);
    fault_exit(&enter(&load), arch::VECTOR_BREAKPOINT, 58)?;
    let exit = enter(&at(arch::step()));
    fault_exit(&exit, arch::VECTOR_DEBUG, 59)?;
    let read_back = read()?;
    let again = continue_();
    fault_exit(&again, arch::VECTOR_AFTER_STEP, 59)?;
    // A jump past the user half: whatever the processor saved, `CONTINUE` either
    // retries it or refuses, and the kernel stays up (B4).
    let mut wild = at(arch::fetch());
    arch::set_first(&mut wild, ferrix_bootinfo_end());
    let exit = enter(&wild);
    hold(exit.reason == Reason::Fault as u64, 56)?;
    let retried = continue_();
    // Either answer, short of a stopped kernel: a processor may report the jump's own
    // address, which retries, or the target, which x86-64 must refuse (ledger 6282 R7).
    let refused_it =
        retried.reason == 0 && retried.number as i64 == status::BAD_STATE.as_return_value() as i64;
    hold(refused_it || retried.reason == Reason::Fault as u64, 56)?;
    // `CONTINUE` after a system call exit is refused: its way back is `RESUME`.
    let mut context = at(arch::sys());
    arch::set_call(&mut context, arch::GETPPID, [0; 6]);
    syscall_exit(&enter(&context), arch::GETPPID, 57)?;
    let refused_continue = continue_();
    hold(
        refused_continue.reason == 0
            && refused_continue.number as i64 == status::BAD_STATE.as_return_value() as i64,
        57,
    )?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 57)?;
    let exit = enter(&at(arch::step()));
    fault_exit(&exit, arch::VECTOR_DEBUG, 59)?;
    let exit = enter(&read_back);
    fault_exit(&exit, arch::VECTOR_AFTER_STEP, 59)?;
    Ok(())
}

/// Cases 60 to 62: the two tables. A number means different things in each.
fn tables(given: &Given) -> Result<(), i32> {
    // From normal mode: a handle in the shared table.
    let port = sys(nr::PORT_CREATE, [0; 6]).map_err(|_| 60)?;
    // From restricted mode the same number names nothing.
    let mut context = at(arch::sys());
    arch::set_call(
        &mut context,
        nr::HANDLE_DUPLICATE as u64,
        [port, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    );
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 61)?;
    hold(
        arch::result_of(&read()?) as i64 == status::BAD_HANDLE.as_return_value() as i64,
        61,
    )?;
    // A handle made from restricted mode is in the private table: normal mode
    // cannot name it, unless a number happens to mean something there too.
    let mut context = at(arch::sys());
    arch::set_call(&mut context, nr::PORT_CREATE as u64, [0; 6]);
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 62)?;
    let private = arch::result_of(&read()?);
    hold((private as i64) > 0, 62)?;
    let _ = given;
    PRIVATE_PORT.store(private, core::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// The private port [`tables`] made, for [`moves`].
static PRIVATE_PORT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Cases 63 to 68: the two moves.
fn moves() -> Result<(), i32> {
    let private = PRIVATE_PORT.load(core::sync::atomic::Ordering::Relaxed);
    // Wider rights than it holds.
    refused(
        sys(
            nr::HANDLE_TO_SHARED,
            [private, u64::from(Rights::ALL.0), 0, 0, 0, 0],
        ),
        status::INVALID_ARGS,
        63,
    )?;
    // A number not in the private table.
    refused(
        sys(
            nr::HANDLE_TO_SHARED,
            [0x7777, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
        ),
        status::BAD_HANDLE,
        64,
    )?;
    // KEEP, which a port's DUPLICATE allows: the private one stays.
    let shared = sys(
        nr::HANDLE_TO_SHARED,
        [private, u64::from(SAME_RIGHTS), HANDLE_KEEP as u64, 0, 0, 0],
    )
    .map_err(|_| 65)?;
    // A move proper: the private one is gone.
    let moved = sys(
        nr::HANDLE_TO_SHARED,
        [private, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    )
    .map_err(|_| 66)?;
    refused(
        sys(
            nr::HANDLE_TO_SHARED,
            [private, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
        ),
        status::BAD_HANDLE,
        66,
    )?;
    // And back again, into the private table.
    let back = sys(
        nr::HANDLE_TO_PRIVATE,
        [moved, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    )
    .map_err(|_| 67)?;
    refused(
        sys(nr::HANDLE_CLOSE, [moved, 0, 0, 0, 0, 0]),
        status::BAD_HANDLE,
        67,
    )?;
    let _ = sys(nr::HANDLE_CLOSE, [shared, 0, 0, 0, 0, 0]);
    // No TRANSFER: a handle reduced to `WAIT` from restricted mode.
    let mut context = at(arch::sys());
    arch::set_call(
        &mut context,
        nr::HANDLE_REPLACE as u64,
        [back, u64::from(Rights::WAIT.0), 0, 0, 0, 0],
    );
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 68)?;
    let reduced = arch::result_of(&read()?);
    refused(
        sys(
            nr::HANDLE_TO_SHARED,
            [reduced, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
        ),
        status::ACCESS_DENIED,
        68,
    )?;
    Ok(())
}

/// Cases 70 to 72: `linuxd`'s copies: through the normal root at a program's
/// memory, a slot synced on the way, and a hole lands.
fn copy_window() -> Result<(), i32> {
    // The landing outside the region is refused.
    let code = (run as *const ()).addr() as u64;
    refused(
        sys(
            nr::RESTRICTED_COPY_WINDOW,
            [code, code + 4, 0x1000, 0, 0, 0],
        ),
        status::INVALID_ARGS,
        70,
    )?;
    // A landing at the region's very end is no address at all (B1).
    refused(
        sys(
            nr::RESTRICTED_COPY_WINDOW,
            [code, code + 4, ferrix_bootinfo_end(), 0, 0, 0],
        ),
        status::INVALID_ARGS,
        69,
    )?;
    declare_copy_window().map_err(|_| 70)?;
    hold(copy_word(ZERO_AT) == Ok(7), 71)?;
    hold(copy_word(HOLE) == Err(FaultKind::NoMapping as u64), 72)
}

/// One past the region: `USER_VIRT_END`, which is not canonical.
const fn ferrix_bootinfo_end() -> u64 {
    0x8000_0000_0000
}

/// A word of the program's memory, by the runtime's copy.
fn copy_word(from: u64) -> Result<u64, u64> {
    let mut word = [0_u8; 8];
    copy_from_program(&mut word, from).map(|()| u64::from_ne_bytes(word))
}

/// Cases 74 to 77: a program's vector registers are its own, and an image is
/// checked.
fn vectors() -> Result<(), i32> {
    if !arch::has_vectors() {
        return Ok(());
    }
    let mut context = at(arch::vectors());
    arch::set_first(&mut context, 0x1234_5678);
    arch::set_second(&mut context, arch::GETPPID);
    let exit = enter(&context);
    syscall_exit(&exit, arch::GETPPID, 74)?;
    // This mode's own values in the same registers while it answers.
    arch::clobber_vectors(0xDEAD_BEEF);
    let exit = resume(0);
    fault_exit(&exit, arch::VECTOR_BREAKPOINT, 75)?;
    let back = read()?;
    hold(arch::result_of(&back) == 0x1234_5678, 75)?;
    hold(arch::second_result_of(&back) == 0x1234_5678, 76)?;
    let mut image = [0_u8; ferrix_native_abi::restricted::VECTORS_BYTES];
    let read_bytes =
        ferrix_rt::native::region::vectors_read(ferrix_rt::Kernel, &mut image).map_err(|_| 77)?;
    hold(read_bytes == image.len(), 77)?;
    let good = ferrix_rt::native::region::vectors_write(ferrix_rt::Kernel, &image);
    hold(good.is_ok(), 77)?;
    // `MXCSR` with its reserved bits set is refused, not masked.
    image[arch::VECTOR_BAD_AT + 2] = 0xFF;
    let bad = ferrix_rt::native::region::vectors_write(ferrix_rt::Kernel, &image);
    hold(bad.is_err(), 78)
}

/// Case 79: a 32-bit context: `int $0x80` exits zero-extended, and its
/// descriptor reads back through `gs`.
fn compat() -> Result<(), i32> {
    let context = arch::compat_context(IMAGE_AT + arch::compat(), STACK_TOP - 64, ZERO_AT as u32);
    let exit = enter(&context);
    hold(exit.reason == Reason::SyscallCompat as u64, 79)?;
    hold(exit.number == arch::I386_GETPID, 79)?;
    hold(exit.words[0] == 7, 79)?;
    hold(exit.words[1] == 0x2222 && exit.words[5] == 0x6666, 79)?;
    fault_exit(&resume(9), arch::VECTOR_BREAKPOINT, 79)
}

/// Case 80: under `exit`, one barrier per exit and none per entry: the check
/// counts around a hundred trips.
fn barrier(given: &Given) -> Result<(), i32> {
    let mut context = at(arch::sys());
    arch::set_call(&mut context, arch::GETPPID, [0; 6]);
    syscall_exit(&enter(&context), arch::GETPPID, 80)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 80)?;
    ask(given, ASK_COUNT_START, 0, None)?;
    const TRIPS: u64 = 100;
    let mut left = TRIPS;
    while left > 0 {
        syscall_exit(&enter(&context), arch::GETPPID, 81)?;
        fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 81)?;
        left -= 1;
    }
    // Two exits a trip.
    ask(given, ASK_COUNT_END, TRIPS * 2, None).map_err(|_| 82)
}

/// A second thread's stack.
#[repr(align(16))]
struct Stack(core::cell::UnsafeCell<[u8; 0x8000]>);
// SAFETY: one thread uses it at a time, by this file's order.
unsafe impl Sync for Stack {}
/// [`Stack`] for the kicked thread.
static KICKED_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// [`Stack`] for the second process's first thread.
static SECOND_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));

/// The stack's top.
fn top(stack: &Stack) -> u64 {
    stack.0.get().addr() as u64 + 0x8000
}

/// What the kicked thread has seen: 0 not started, its step, or a failure.
static KICK_STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The kicked thread's body: spin in restricted mode until an exit, and say
/// which exit it was.
extern "C" fn kicked(_argument: usize) -> ! {
    use core::sync::atomic::Ordering::{Acquire, Release};
    let exit = enter(&at(arch::spin()));
    KICK_STATE.store(
        if exit.reason == Reason::Kick as u64 {
            2
        } else {
            100
        },
        Release,
    );
    // Back in, in normal mode; a kick now is for the next entry.
    while KICK_STATE.load(Acquire) != 3 {
        core::hint::spin_loop();
    }
    let exit = continue_();
    KICK_STATE.store(
        if exit.reason == Reason::Kick as u64 {
            4
        } else {
            101
        },
        Release,
    );
    // Many kicks while it spins, one exit each time it looks: one more, and it
    // stops by itself.
    let mut left = 3;
    while left > 0 {
        let exit = continue_();
        if exit.reason != Reason::Kick as u64 {
            KICK_STATE.store(102, Release);
        }
        KICK_STATE.store(10 + left, Release);
        left -= 1;
        while KICK_STATE.load(Acquire) == 10 + left + 1 {
            core::hint::spin_loop();
        }
    }
    KICK_STATE.store(5, Release);
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// Kick `thread`, which may be ending by itself as this looks: that refusal
/// is no failure, and any other is `case`.
fn kick_racing(thread: u64, case: i32) -> Result<(), i32> {
    match sys(nr::RESTRICTED_KICK, [thread, 0, 0, 0, 0, 0]) {
        Err(why) if why != status::BAD_STATE => Err(case),
        _ => Ok(()),
    }
}

/// Wait for [`KICK_STATE`] to reach `want`, kicking `thread` while it has not,
/// when `kick`.
fn await_state(want: u64, thread: u64, kick: bool, case: i32) -> Result<(), i32> {
    use core::sync::atomic::Ordering::Acquire;
    let mut spins = 0_u64;
    loop {
        let state = KICK_STATE.load(Acquire);
        if state == want {
            return Ok(());
        }
        if state >= 100 {
            return Err(case);
        }
        if kick {
            kick_racing(thread, case)?;
        }
        spins += 1;
        if spins > 100_000_000 {
            return Err(case);
        }
        core::hint::spin_loop();
    }
}

/// Cases 84 to 87: kicks: in restricted mode; in normal mode, for the next
/// entry; and `SIGNAL` needed.
fn kicks() -> Result<(), i32> {
    use core::sync::atomic::Ordering::Release;
    let thread = sys(
        nr::THREAD_CREATE,
        [
            (kicked as *const ()).addr() as u64,
            top(&KICKED_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 84)?;
    // No `SIGNAL`: refused.
    let weak = sys(
        nr::HANDLE_DUPLICATE,
        [thread, u64::from(Rights::WAIT.0), 0, 0, 0, 0],
    )
    .map_err(|_| 84)?;
    refused(
        sys(nr::RESTRICTED_KICK, [weak, 0, 0, 0, 0, 0]),
        status::ACCESS_DENIED,
        84,
    )?;
    await_state(2, thread, true, 85)?;
    // A thread in normal mode: the kick waits, its next entry exits at once.
    kick_racing(thread, 86)?;
    KICK_STATE.store(3, Release);
    await_state(4, thread, false, 86)?;
    // In a spin again: a storm of kicks is one exit at a time.
    await_state(13, thread, true, 87)?;
    KICK_STATE.store(14, Release);
    await_state(12, thread, true, 87)?;
    KICK_STATE.store(13, Release);
    await_state(11, thread, true, 87)?;
    KICK_STATE.store(12, Release);
    await_state(5, thread, false, 87)?;
    // The thread's end is what its handle waits on.
    let mut observed = 0_u32;
    let _ = sys(
        nr::OBJECT_WAIT_ONE,
        [
            thread,
            u64::from(ferrix_native_abi::signals::Signals::TERMINATED.0),
            0,
            (&raw mut observed).addr() as u64,
            0,
            0,
        ],
    )
    .map_err(|_| 88)?;
    refused(
        sys(nr::RESTRICTED_KICK, [thread, 0, 0, 0, 0, 0]),
        status::BAD_STATE,
        88,
    )
}

/// Cases 120 to 122: pointer routing. From normal mode a buffer in either half
/// copies right; from restricted mode a buffer at a region address does not.
fn pointers() -> Result<(), i32> {
    let vmo = sys(nr::VMO_CREATE, [0x1000, 0, 0, 0, 0, 0]).map_err(|_| 120)?;
    let offset = 0_u64;
    // From the restricted half: `ZERO_AT` holds 7 by now.
    sys(
        nr::VMO_WRITE,
        [vmo, ZERO_AT, 8, (&raw const offset).addr() as u64, 0, 0],
    )
    .map_err(|_| 120)?;
    // Into the region.
    let mut word = 0_u64;
    sys(
        nr::VMO_READ,
        [
            vmo,
            (&raw mut word).addr() as u64,
            8,
            (&raw const offset).addr() as u64,
            0,
            0,
        ],
    )
    .map_err(|_| 121)?;
    hold(word == 7, 121)?;
    // A restricted call naming the region is refused: this program's own word.
    let mut context = at(arch::sys());
    arch::set_call(&mut context, nr::PORT_CREATE as u64, [0; 6]);
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 122)?;
    let port = arch::result_of(&read()?);
    let packet = (&raw const PRIVATE_PORT).addr() as u64;
    let mut context = at(arch::sys());
    arch::set_call(
        &mut context,
        nr::PORT_QUEUE as u64,
        [port, packet, 0, 0, 0, 0],
    );
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 122)?;
    hold(
        arch::result_of(&read()?) as i64 == status::FAULT.as_return_value() as i64,
        122,
    )?;
    PRIVATE_PORT.store(port, core::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// A stack for [`spinner`].
static SPINNER_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// What [`spinner`] saw: 2 for a kick in its spin, 1 for anything else.
static SPINNER_DONE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Spin in restricted mode until an exit; say how it was.
extern "C" fn spinner(_argument: usize) -> ! {
    let exit = enter(&at(arch::spin()));
    let done = if exit.reason == Reason::Kick as u64 && read().is_ok() {
        2
    } else {
        1
    };
    SPINNER_DONE.store(done, core::sync::atomic::Ordering::Release);
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// Cases 124 to 126: a thread preempted in restricted mode comes back there,
/// and a switch from it to a normal-mode thread of the same process is an exit
/// by another name (`docs/microkernel/K1.md` §6, `L.x86_64.276`): pinned to one
/// processor, this thread's wakes count barriers with no exit in the window.
fn preempted(given: &Given) -> Result<(), i32> {
    use core::sync::atomic::Ordering::Acquire;
    let thread = sys(
        nr::THREAD_CREATE,
        [
            (spinner as *const ()).addr() as u64,
            top(&SPINNER_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 124)?;
    let pinned = sys(
        nr::HANDLE_DUPLICATE,
        [thread, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
    )
    .map_err(|_| 124)?;
    ask(given, ASK_PIN, 0, Some(pinned))?;
    let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 124)?;
    ask(given, ASK_PIN, 0, Some(me))?;
    ask(given, ASK_COUNT_START, 0, None)?;
    // Block until the spinner has run in restricted mode here and this thread
    // has come back over it, in normal mode: the spinner reaches this processor
    // at a way out of its own, so a bound and not a count.
    let mut left = 100_000_u32;
    while ask(given, ASK_COUNT_MOVED, 0, None).is_err() {
        left = left.checked_sub(1).ok_or(125)?;
        ask(given, ASK_NOTHING, 0, None)?;
    }
    // The spinner is still in restricted mode: a kick brings it out of its
    // loop.
    let mut spins = 0_u64;
    while SPINNER_DONE.load(Acquire) == 0 {
        kick_racing(thread, 126)?;
        spins += 1;
        if spins > 100_000_000 {
            return Err(126);
        }
        core::hint::spin_loop();
    }
    hold(SPINNER_DONE.load(Acquire) == 2, 126)
}

/// A stack for [`waiter`].
static WAITER_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// What [`waiter`] saw.
static WAITER_STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Wait on a port from restricted mode, with no deadline.
extern "C" fn waiter(_argument: usize) -> ! {
    use core::sync::atomic::Ordering::{Relaxed, Release};
    let port = PRIVATE_PORT.load(Relaxed);
    let mut context = at(arch::sys());
    arch::set_call(
        &mut context,
        nr::PORT_WAIT as u64,
        [port, 0, ZERO_AT + 16, 0, 0, 0],
    );
    WAITER_STATE.store(1, Release);
    // A kick before the call began exits with the call still to make, and
    // `RESTRICTED_CONTINUE` makes it: so until the wait itself is what was ended.
    let mut exit = enter(&context);
    let state = loop {
        if exit.reason != Reason::Kick as u64 {
            break 100;
        }
        match read() {
            Ok(back)
                if arch::result_of(&back) as i64
                    == status::INTERRUPTED.as_return_value() as i64 =>
            {
                break 2;
            }
            Ok(_) => exit = continue_(),
            Err(_) => break 101,
        }
    };
    WAITER_STATE.store(state, Release);
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// A stack for [`round_trip_waiter`].
static CHANNEL_WAITER_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// What [`round_trip_waiter`] saw.
static CHANNEL_WAITER_STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The private channel end it waits on.
static CHANNEL_END: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Wait on `channel_write_read`'s receive half from restricted mode: the path a
/// kick must end as well (B2).
extern "C" fn round_trip_waiter(_argument: usize) -> ! {
    use core::sync::atomic::Ordering::{Relaxed, Release};
    let channel = CHANNEL_END.load(Relaxed);
    let mut context = at(arch::sys());
    arch::set_call(
        &mut context,
        nr::CHANNEL_WRITE_READ as u64,
        [channel, 0, 0, 0, 0, 0],
    );
    CHANNEL_WAITER_STATE.store(1, Release);
    let mut exit = enter(&context);
    let state = loop {
        if exit.reason != Reason::Kick as u64 {
            break 100;
        }
        match read() {
            Ok(back)
                if arch::result_of(&back) as i64
                    == status::INTERRUPTED.as_return_value() as i64 =>
            {
                break 2;
            }
            Ok(_) => exit = continue_(),
            Err(_) => break 101,
        }
    };
    CHANNEL_WAITER_STATE.store(state, Release);
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// Case 136: a kick ends a blocked `channel_write_read` from restricted mode.
fn interrupted_round_trip() -> Result<(), i32> {
    use core::sync::atomic::Ordering::{Acquire, Relaxed};
    let mut context = at(arch::sys());
    arch::set_call(
        &mut context,
        nr::CHANNEL_CREATE as u64,
        [ZERO_AT + 32, 0, 0, 0, 0, 0],
    );
    fault_exit(&enter(&context), arch::VECTOR_BREAKPOINT, 136)?;
    let first = copy_word(ZERO_AT + 32).map_err(|_| 136)?;
    CHANNEL_END.store(first & 0xFFFF_FFFF, Relaxed);
    let thread = sys(
        nr::THREAD_CREATE,
        [
            (round_trip_waiter as *const ()).addr() as u64,
            top(&CHANNEL_WAITER_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 136)?;
    let mut spins = 0_u64;
    loop {
        match CHANNEL_WAITER_STATE.load(Acquire) {
            2 => return Ok(()),
            state if state >= 100 => return Err(136),
            1 => kick_racing(thread, 136)?,
            _ => {}
        }
        spins += 1;
        if spins > 100_000_000 {
            return Err(136);
        }
        core::hint::spin_loop();
    }
}

/// Case 127: a kick ends a blocking native call from restricted mode with
/// `INTERRUPTED`, then exits.
fn interrupted_wait() -> Result<(), i32> {
    use core::sync::atomic::Ordering::Acquire;
    let thread = sys(
        nr::THREAD_CREATE,
        [
            (waiter as *const ()).addr() as u64,
            top(&WAITER_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 127)?;
    let mut spins = 0_u64;
    loop {
        match WAITER_STATE.load(Acquire) {
            2 => return Ok(()),
            state if state >= 100 => return Err(127),
            1 => {
                kick_racing(thread, 127)?;
            }
            _ => {}
        }
        spins += 1;
        if spins > 100_000_000 {
            return Err(127);
        }
        core::hint::spin_loop();
    }
}

/// A word both processes of the region write: case 89.
static SHARED_WORD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Where the case's page is in the region.
static PAGE_AT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Where [`second`] is: 1 its TLB holds the page; 2 the page has changed.
static PAGE_STEP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// [`second`]'s own thread handle, in the shared table, plus one.
static SECOND_THREAD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The second process's first thread: it sees the region, not the first
/// process's restricted space, and a change of the region's mappings reaches
/// its processor.
extern "C" fn second(_argument: usize) -> ! {
    use core::sync::atomic::Ordering::{Acquire, Release};
    let verdict = (|| -> Result<(), i32> {
        // The first process's restricted memory is not here.
        hold(copy_word(ZERO_AT) == Err(FaultKind::NoMapping as u64), 1)?;
        hold(SHARED_WORD.load(Acquire) == 0x51, 2)?;
        SHARED_WORD.store(0x52, Release);
        let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 5)?;
        SECOND_THREAD.store(me + 1, Release);
        // Each round: a page the first process mapped since this one started, read
        // through this processor's TLB just before the unmap, by handshake; and read
        // again once it has changed, with nothing else here to flush it
        // (`L.user.183`). Any stale read fails.
        let mut round = 0_u64;
        while round < SHOOTDOWN_ROUNDS {
            let step = round * 4;
            while PAGE_STEP.load(Acquire) != step + 1 {
                core::hint::spin_loop();
            }
            let at = PAGE_AT.load(Acquire);
            // SAFETY: mapped by the first process before it published this step, and
            // unmapped only once this thread has said it is past this read.
            let before = unsafe { core::ptr::read_volatile(at as *const u64) };
            hold(before == 0xAAAA + round, 3)?;
            PAGE_STEP.store(step + 2, Release);
            while PAGE_STEP.load(Acquire) != step + 3 {
                core::hint::spin_loop();
            }
            // SAFETY: mapped again before the step was published, and unmapped only
            // once this thread has said it is past this read.
            let after = unsafe { core::ptr::read_volatile(at as *const u64) };
            hold(after == 0xAAAA + round + 1, 4)?;
            PAGE_STEP.store(step + 4, Release);
            round += 1;
        }
        Ok(())
    })();
    let status = verdict.map_or_else(|case| case, |()| 0);
    if status != 0 {
        // The first process waits on a step: tell it there will be none.
        PAGE_STEP.store(PAGE_STEP_FAILED, Release);
    }
    let _ = sys(nr::THREAD_EXIT, [status as u64, 1, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// A VMO of one page holding `value`, mapped in the region at `at`, or wherever
/// there is room if that is zero: where it is.
fn page_with(value: u64, at: u64) -> Result<u64, i32> {
    let vmo = sys(nr::VMO_CREATE, [0x1000, 0, 0, 0, 0, 0]).map_err(|_| 128)?;
    let zero = 0_u64;
    sys(
        nr::VMO_WRITE,
        [
            vmo,
            (&raw const value).addr() as u64,
            8,
            (&raw const zero).addr() as u64,
            0,
            0,
        ],
    )
    .map_err(|_| 128)?;
    let protection = u64::from(ferrix_native_abi::types::MAP_READ);
    sys(
        nr::VMO_MAP,
        [
            vmo,
            at,
            0x1000,
            protection,
            (&raw const zero).addr() as u64,
            0,
        ],
    )
    .map_err(|_| 129)
}

/// The restricted space, for the unmapping thread of case 138.
static SHOOTDOWN_SPACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Case 138's steps: 1 to unmap, 2 done, 100 refused.
static SHOOTDOWN_STEP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Where case 138's page is: a root slot of its own.
const SHOOTDOWN_AT: u64 = 0x0200_0000_0000;
/// Case 138's thread's stack.
static SHOOTDOWN_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));

/// Case 138's other thread: unmap the page once asked.
extern "C" fn unmapper(_argument: usize) -> ! {
    use core::sync::atomic::Ordering::{Acquire, Release};
    while SHOOTDOWN_STEP.load(Acquire) != 1 {
        core::hint::spin_loop();
    }
    let space = SHOOTDOWN_SPACE.load(Acquire);
    let done = sys(nr::SPACE_UNMAP, [space, SHOOTDOWN_AT, 0x1000, 0, 0, 0]);
    SHOOTDOWN_STEP.store(if done.is_ok() { 2 } else { 100 }, Release);
    ferrix_rt::exit(0)
}

/// Case 138: a restricted page read here, so held under the restricted root's tag,
/// and unmapped from another processor while this one is in normal mode under
/// the other tag: the next read faults, as `L.aarch64.80` says of a tag not in
/// force, and `L.user.183` of the region's. QEMU's TCG drops everything at a tag's
/// change, so a real core's answer is the board row's.
fn restricted_shootdown(given: &Given) -> Result<(), i32> {
    use core::sync::atomic::Ordering::{Acquire, Release};
    SHOOTDOWN_SPACE.store(given.space, Release);
    let write = u64::from(ferrix_native_abi::types::MAP_READ | ferrix_native_abi::types::MAP_WRITE);
    sys(
        nr::SPACE_ALLOCATE,
        [given.space, SHOOTDOWN_AT, 0x1000, write, 0, 0],
    )
    .map_err(|_| 138)?;
    let thread = sys(
        nr::THREAD_CREATE,
        [
            (unmapper as *const ()).addr() as u64,
            top(&SHOOTDOWN_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 138)?;
    ask(given, ASK_PIN, 1, Some(thread))?;
    let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 138)?;
    ask(given, ASK_PIN, 0, Some(me))?;
    let mut touch = at(arch::touch());
    arch::set_first(&mut touch, SHOOTDOWN_AT);
    fault_exit(&enter(&touch), arch::VECTOR_BREAKPOINT, 138)?;
    let mut load = at(arch::load());
    arch::set_first(&mut load, SHOOTDOWN_AT);
    fault_exit(&enter(&load), arch::VECTOR_BREAKPOINT, 138)?;
    SHOOTDOWN_STEP.store(1, Release);
    loop {
        match SHOOTDOWN_STEP.load(Acquire) {
            2 => break,
            100 => return Err(138),
            _ => core::hint::spin_loop(),
        }
    }
    let after = enter(&load);
    hold(
        after.reason == Reason::Fault as u64 && after.number != arch::VECTOR_BREAKPOINT,
        138,
    )?;
    sys(nr::HANDLE_CLOSE, [thread, 0, 0, 0, 0, 0]).map_err(|_| 138)?;
    Ok(())
}

/// Set by a thread case 141 must not have started.
static RAN_THROUGH_A_FULL_TABLE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// Its stack.
static FULL_TABLE_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));

/// What case 141's thread does, if it ever runs.
extern "C" fn through_a_full_table(_argument: usize) -> ! {
    RAN_THROUGH_A_FULL_TABLE.store(true, core::sync::atomic::Ordering::Release);
    ferrix_rt::exit(0)
}

/// Case 141: with the region's table full, `THREAD_CREATE` is `NO_HANDLES` and nothing
/// ran. The table is a kernel constant's size, so filling it is a loop.
fn full_table_runs_no_thread() -> Result<(), i32> {
    /// More than `HANDLE_LIMIT`, to catch a table that never fills.
    const MOST: usize = 5000;
    use core::sync::atomic::Ordering::Relaxed;
    static HELD: [core::sync::atomic::AtomicU64; MOST] =
        [const { core::sync::atomic::AtomicU64::new(0) }; MOST];
    let mut count = 0;
    let port = sys(nr::PORT_CREATE, [0; 6]).map_err(|_| 141)?;
    while count < MOST {
        match sys(
            nr::HANDLE_DUPLICATE,
            [port, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
        ) {
            Ok(handle) => {
                if let Some(slot) = HELD.get(count) {
                    slot.store(handle, Relaxed);
                }
                count += 1;
            }
            Err(_) => break,
        }
    }
    hold(count < MOST, 141)?;
    let answer = sys(
        nr::THREAD_CREATE,
        [
            (through_a_full_table as *const ()).addr() as u64,
            top(&FULL_TABLE_STACK),
            0,
            0,
            0,
            0,
        ],
    );
    for handle in HELD.iter().take(count) {
        sys(nr::HANDLE_CLOSE, [handle.load(Relaxed), 0, 0, 0, 0, 0]).map_err(|_| 141)?;
    }
    sys(nr::HANDLE_CLOSE, [port, 0, 0, 0, 0, 0]).map_err(|_| 141)?;
    refused(answer, status::NO_HANDLES, 141)?;
    // Long past any start, and it answers on this processor or another.
    let mut left = 2_000_000_u32;
    while left > 0 {
        core::hint::spin_loop();
        left -= 1;
    }
    hold(
        !RAN_THROUGH_A_FULL_TABLE.load(core::sync::atomic::Ordering::Acquire),
        141,
    )
}

/// The reader of case 132 has stopped: no further step will come.
const PAGE_STEP_FAILED: u64 = u64::MAX;
/// Rounds of case 132's change of a page under another processor's reader.
const SHOOTDOWN_ROUNDS: u64 = 64;

/// Case 145: under `ENTER_ALL_CALLS_EXIT` a native number exits like any other and
/// answers nothing, across a `RESUME`, until the next entry; an unknown option is
/// refused with nothing taken (6268 Q2, ledger 6663).
fn all_calls_exit() -> Result<(), i32> {
    let mut context = at(arch::sys_twice());
    arch::set_call(&mut context, nr::THREAD_SELF as u64, [0; 6]);
    syscall_exit(
        &ferrix_rt::restricted::enter_all_calls_exit(&context),
        nr::THREAD_SELF as u64,
        145,
    )?;
    // Refused, it takes nothing: the context held is still this one, its flag up.
    let mut other = at(arch::sys());
    arch::set_call(&mut other, nr::THREAD_SELF as u64, [0; 6]);
    let refused_entry = ferrix_rt::restricted::enter_with(&other, 1 << 63);
    hold(
        refused_entry.reason == 0
            && refused_entry.number as i64 == status::INVALID_ARGS.as_return_value() as i64,
        145,
    )?;
    // The second call, a native number again, past the `RESUME`: on x86-64 the number is
    // the value resumed, in RAX; on AArch64 X8 still holds the first's.
    syscall_exit(&resume(nr::THREAD_SELF as u64), nr::THREAD_SELF as u64, 145)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 145)?;
    let mut once = at(arch::sys());
    arch::set_call(&mut once, nr::THREAD_SELF as u64, [0; 6]);
    let plain = enter(&once);
    hold(plain.reason != Reason::Syscall as u64, 145)?;
    Ok(())
}

/// Case 89: a second process of the region: the same shared word, its own
/// restricted space and table.
#[allow(clippy::too_many_lines, reason = "one handshake, in order")]
fn second_process(given: &Given) -> Result<(), i32> {
    use core::sync::atomic::Ordering::{Acquire, Release};
    SHARED_WORD.store(0x51, Release);
    let mut out = 0_u64;
    let name = b"second";
    let process = sys(
        nr::PROCESS_CREATE_SHARED,
        [
            given.region,
            given.job,
            0,
            name.as_ptr().addr() as u64,
            name.len() as u64,
            (&raw mut out).addr() as u64,
        ],
    )
    .map_err(|_| 89)?;
    sys(
        nr::PROCESS_START_SHARED,
        [
            process,
            (second as *const ()).addr() as u64,
            top(&SECOND_STACK),
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 89)?;
    // The page's change, while the second process's processor holds it, with the two
    // on processors of their own, and shown to be. With one processor there is no such
    // case to run (`L.user.183`): the check's line says so.
    let mut thread = 0;
    while thread == 0 {
        thread = SECOND_THREAD.load(Acquire);
        core::hint::spin_loop();
    }
    let two = asked(given, ASK_TWO_PROCESSORS, 0, None)?;
    if two {
        let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 89)?;
        ask(given, ASK_PIN, 0, Some(me))?;
        // `ASK_PIN` takes the handle it is given, and the region's table is shared, so
        // `SECOND_THREAD`'s number means the same thread here.
        let again = sys(
            nr::HANDLE_DUPLICATE,
            [thread - 1, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
        )
        .map_err(|_| 89)?;
        ask(given, ASK_PIN, 1, Some(again))?;
        let mut moved = false;
        let mut left = 200_000_u32;
        while !moved && left > 0 {
            let again = sys(
                nr::HANDLE_DUPLICATE,
                [thread - 1, u64::from(SAME_RIGHTS), 0, 0, 0, 0],
            )
            .map_err(|_| 89)?;
            moved = asked(given, ASK_ON_PROCESSOR, 1, Some(again))?;
            left -= 1;
        }
        hold(moved, 132)?;
    }
    // Until the reader reaches `step`, or has found a stale word and stopped.
    let reader_at = |step: u64| {
        loop {
            match PAGE_STEP.load(Acquire) {
                PAGE_STEP_FAILED => return Err(132),
                reached if reached == step => return Ok(()),
                _ => core::hint::spin_loop(),
            }
        }
    };
    let mut at = 0;
    let mut round = 0_u64;
    while round < SHOOTDOWN_ROUNDS {
        let step = round * 4;
        let mapped = page_with(0xAAAA + round, at)?;
        hold(at == 0 || mapped == at, 131)?;
        at = mapped;
        PAGE_AT.store(at, Release);
        PAGE_STEP.store(step + 1, Release);
        reader_at(step + 2)?;
        sys(nr::SPACE_UNMAP, [given.region_space, at, 0x1000, 0, 0, 0]).map_err(|_| 130)?;
        let again = page_with(0xAAAA + round + 1, at)?;
        hold(again == at, 131)?;
        PAGE_STEP.store(step + 3, Release);
        reader_at(step + 4)?;
        sys(nr::SPACE_UNMAP, [given.region_space, at, 0x1000, 0, 0, 0]).map_err(|_| 130)?;
        round += 1;
    }
    if two {
        let me = sys(nr::THREAD_SELF, [0; 6]).map_err(|_| 89)?;
        ask(given, ASK_PIN, ANY_PROCESSOR, Some(me))?;
    }
    let mut observed = 0_u32;
    let _ = sys(
        nr::OBJECT_WAIT_ONE,
        [
            process,
            u64::from(ferrix_native_abi::signals::Signals::TERMINATED.0),
            0,
            (&raw mut observed).addr() as u64,
            0,
            0,
        ],
    )
    .map_err(|_| 89)?;
    let mut ended = [0_u32; 2];
    sys(
        nr::PROCESS_STATUS,
        [process, ended.as_mut_ptr().addr() as u64, 0, 0, 0, 0],
    )
    .map_err(|_| 89)?;
    hold(ended[1] == 0, 132)?;
    hold(SHARED_WORD.load(Acquire) == 0x52, 89)
}

/// Where the row's samples go: its own root slot's worth of the window.
const SAMPLES_AT: u64 = 0x1400_0000;
/// Untimed round trips before the timed ones.
const ROW_WARMUP: u64 = 2_000;
/// Timed round trips a repetition: `bench-rows`' count.
const ROW_SAMPLES: u64 = 20_000;
/// Repetitions: `bench-rows`' count.
const ROW_REPEATS: usize = 5;

/// The restricted row (`MICROKERNEL.md` §7): `getppid`'s number through restricted
/// mode and straight back, as the program times it, the median of five p50s. The
/// check scales it to nanoseconds and says it on its line, which `bench-rows`
/// reads, as it reads the block row off the `seam` line.
fn row(given: &Given) -> Result<(), i32> {
    let bytes = (ROW_WARMUP + ROW_SAMPLES) * 8;
    let write = u64::from(ferrix_native_abi::types::MAP_READ | ferrix_native_abi::types::MAP_WRITE);
    sys(
        nr::SPACE_ALLOCATE,
        [
            given.space,
            SAMPLES_AT,
            (bytes + 0xFFF) & !0xFFF,
            write,
            0,
            0,
        ],
    )
    .map_err(|_| 135)?;
    ask(given, ASK_ROW_START, 0, None)?;
    let started = arch::ticks();
    let mut p50s = [0_u64; ROW_REPEATS];
    for p50 in &mut p50s {
        let mut context = at(arch::bench());
        arch::set_bench(&mut context, ROW_WARMUP + ROW_SAMPLES, SAMPLES_AT);
        let mut exit = enter(&context);
        while exit.reason == Reason::Syscall as u64 {
            exit = resume(1);
        }
        fault_exit(&exit, arch::VECTOR_BREAKPOINT, 135)?;
        // SAFETY: the program's memory, which the normal root maps, every page
        // touched by the loop that just ended and nothing else using it now.
        let samples = unsafe {
            core::slice::from_raw_parts_mut(
                (SAMPLES_AT + ROW_WARMUP * 8) as *mut u64,
                ROW_SAMPLES as usize,
            )
        };
        samples.sort_unstable();
        *p50 = samples.get(samples.len() / 2).copied().unwrap_or(0);
    }
    ask(given, ASK_ROW_TICKS, arch::ticks() - started, None)?;
    p50s.sort_unstable();
    let median = p50s.get(ROW_REPEATS / 2).copied().unwrap_or(0);
    ask(given, ASK_ROW_P50, median, None)
}

/// A process body that spins in restricted mode.
extern "C" fn spin_restricted(_argument: usize) -> ! {
    let _ = enter(&at(arch::spin()));
    loop {
        core::hint::spin_loop();
    }
}

/// A process body that spins in normal mode.
extern "C" fn spin_normal(_argument: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// A stack for the killed processes: one at a time.
static KILLED_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));

/// Cases 133, 134: a job's kill ends a process of the region in either mode
/// (`L.trap.25`), and leaves this one running.
fn kills(given: &Given) -> Result<(), i32> {
    for (body, case) in [
        (spin_restricted as extern "C" fn(usize) -> !, 133),
        (spin_normal as extern "C" fn(usize) -> !, 134),
    ] {
        let child = sys(nr::JOB_CREATE, [given.job, 0, 0, 0, 0, 0]).map_err(|_| case)?;
        let mut out = 0_u64;
        let name = b"killed";
        let process = sys(
            nr::PROCESS_CREATE_SHARED,
            [
                given.region,
                child,
                0,
                name.as_ptr().addr() as u64,
                name.len() as u64,
                (&raw mut out).addr() as u64,
            ],
        )
        .map_err(|_| case)?;
        // Its restricted space holds what its body runs.
        map_space(out).map_err(|_| case)?;
        sys(
            nr::PROCESS_START_SHARED,
            [
                process,
                (body as *const ()).addr() as u64,
                top(&KILLED_STACK),
                0,
                0,
                0,
            ],
        )
        .map_err(|_| case)?;
        // Let it get going.
        let mut left = 20_u32;
        while left > 0 {
            core::hint::spin_loop();
            left -= 1;
        }
        sys(nr::JOB_KILL, [child, 0, 0, 0, 0, 0]).map_err(|_| case)?;
        let mut observed = 0_u32;
        sys(
            nr::OBJECT_WAIT_ONE,
            [
                process,
                u64::from(ferrix_native_abi::signals::Signals::TERMINATED.0),
                0,
                (&raw mut observed).addr() as u64,
                0,
                0,
            ],
        )
        .map_err(|_| case)?;
    }
    Ok(())
}
