//! The window's cases (`restricted_window`, ledger 6787, 6998), run where the boot decided the
//! machine has one. Numbers 160 to 179.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_rt::restricted::resume_armed;

use super::{
    Context, Given, Reason, Stack, arch, ask, asked, at, continue_, enter, fault_exit, hold, nr,
    resume, sys, syscall_exit, top,
};

/// Whether this machine answers armed calls by a stub.
pub(super) const ASK_WINDOW_ON: u8 = 14;
/// Read the trip and decline counts.
const ASK_WINDOW_START: u8 = 15;
/// The trips since [`ASK_WINDOW_START`] must be the word, or above zero for [`SOME`].
const ASK_WINDOW_TRIPS: u8 = 16;
/// [`ASK_WINDOW_TRIPS`]'s word for any at all.
const SOME: u64 = u64::MAX;
/// Trips and declines since [`ASK_WINDOW_START`] must both be above zero.
const ASK_WINDOW_BOTH: u8 = 17;

/// Read how many windows there are.
const ASK_WINDOW_LIVE: u8 = 18;
/// There must be as many as then, within the check's patience.
const ASK_WINDOW_WENT: u8 = 19;
/// The check reads the window's pages by software, many times, while this thread waits for it.
const ASK_WINDOW_READERS: u8 = 20;

/// The window's root slot's first address (`mm::WINDOW_ROOT_SLOT`).
const STUB: u64 = 255 << 39;
/// The answer's page.
const DATA: u64 = STUB + 0x1000;
/// The page nothing touches (ledger 6787 W6).
const CANARY: u64 = STUB + 0x2000;
/// Rounds of the other thread's rejoining.
const ROUNDS: usize = 300;
/// A long run of armed calls: a kick ends it.
const FOREVER: u64 = u64::MAX;
/// The value the other thread's armed calls answer, and this one's.
const ITS: u64 = 0xA5A5_0001;
/// See [`ITS`].
const MINE: u64 = 0x5A5A_0002;

/// The other thread's stack.
static OTHER_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// What it has seen: 1 armed, 2 kicked, 3 a call again after the kick; else its failure.
static OTHER: AtomicU64 = AtomicU64::new(0);

/// A blocking round trip with the check: this thread leaves its processor, and its bit the set.
fn nap(given: &Given) -> Result<(), i32> {
    ask(given, super::ASK_NOTHING, 0, None)
}

/// Enter `context` and keep arming `GETPPID` with its own value, as `linuxd`'s straight run does,
/// until an exit that is no such call: it and how many such calls did exit. A resume off the
/// general path arms nothing, so the first stub is the third call at the soonest.
fn armed_run(context: &Context) -> (ferrix_rt::restricted::Exit, u64) {
    let mut exit = enter(context);
    let mut exits = 0;
    while exit.reason == Reason::Syscall as u64 && exit.number == arch::GETPPID {
        exits += 1;
        exit = resume_armed(arch::GETPPID, arch::GETPPID);
    }
    (exit, exits)
}

/// Cases 160 to 162: of four armed calls, those that did not exit took stubs, with a barrier each
/// (6787 W10), and every register comes back as it went.
fn trips(given: &Given) -> Result<(), i32> {
    /// The calls in [`arch::armed`].
    const CALLS: u64 = 4;
    ask(given, ASK_WINDOW_START, 0, None)?;
    ask(given, super::ASK_COUNT_START, 0, None)?;
    let mut context = at(arch::armed());
    arch::mark_every_register(&mut context);
    arch::set_call(
        &mut context,
        arch::GETPPID,
        [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
    );
    let (exit, exits) = armed_run(&context);
    fault_exit(&exit, arch::VECTOR_BREAKPOINT, 160)?;
    hold(exits < CALLS, 160)?;
    let back = super::read()?;
    hold(arch::result_of(&back) == arch::GETPPID, 161)?;
    hold(arch::every_register_marked(&back), 161)?;
    ask(given, ASK_WINDOW_TRIPS, CALLS - exits, None).map_err(|_| 160)?;
    // Each call, by stub or not, and the breakpoint.
    ask(given, super::ASK_COUNT_END, CALLS + 1, None).map_err(|_| 162)
}

/// Case 163: `TS` is not set on this path, and nothing of the program's vectors moves: they come
/// back over two stubs, and this thread's own are its own afterwards (ledger 6826).
fn vectors() -> Result<(), i32> {
    if !arch::has_vectors() {
        return Ok(());
    }
    const THEIRS: u64 = 0x1234_5678_9ABC_DEF0;
    const OURS: u64 = 0x0FED_CBA9_8765_4321;
    arch::clobber_vectors(OURS);
    let mut context = at(arch::armed_vectors());
    arch::set_first(&mut context, THEIRS);
    let (exit, _) = armed_run(&context);
    fault_exit(&exit, arch::VECTOR_BREAKPOINT, 163)?;
    let back = super::read()?;
    hold(
        arch::result_of(&back) == THEIRS && arch::second_result_of(&back) == THEIRS,
        163,
    )?;
    hold(arch::read_vectors() == OURS, 163)
}

/// Cases 164 to 166: once a stub has closed, the program reaches nothing of it: not its text, not
/// its answer, and not the page under the same tables that nothing touched, which only a
/// kept upper-level entry could show (ledger 6787 W6).
fn closed() -> Result<(), i32> {
    for (address, case) in [(STUB, 164), (DATA, 165), (CANARY, 166)] {
        let mut context = at(arch::armed_load());
        arch::set_call(&mut context, arch::GETPPID, [address, 0, 0, 0, 0, 0]);
        let (exit, _) = armed_run(&context);
        fault_exit(&exit, arch::VECTOR_PAGE, case)?;
    }
    Ok(())
}

/// Case 167: any other number drops the arm: the armed number exits again after it.
fn other_number() -> Result<(), i32> {
    let mut context = at(arch::armed_other());
    arch::set_call(&mut context, arch::GETPPID, [0; 6]);
    let (exit, _) = armed_run(&context);
    syscall_exit(&exit, 1000, 167)?;
    let exit = resume(0);
    syscall_exit(&exit, arch::GETPPID, 167)?;
    let exit = resume(0);
    fault_exit(&exit, arch::VECTOR_BREAKPOINT, 167)
}

/// The other thread: armed calls until a kick, which must drop the arm, then one more call.
extern "C" fn other(_argument: usize) -> ! {
    let mut context = at(arch::armed_loop());
    arch::set_armed_loop(&mut context, FOREVER, ITS);
    let exit = enter(&context);
    if exit.reason != Reason::Syscall as u64 {
        OTHER.store(100, Ordering::Release);
    } else {
        OTHER.store(1, Ordering::Release);
        let mut exit = resume_armed(ITS, arch::GETPPID);
        while exit.reason == Reason::Syscall as u64 {
            exit = resume_armed(ITS, arch::GETPPID);
        }
        // Only a kick ends the run: `ud2` would be another's word.
        OTHER.store(
            if exit.reason == Reason::Kick as u64 {
                2
            } else {
                101
            },
            Ordering::Release,
        );
        let exit = continue_();
        let again = exit.reason == Reason::Syscall as u64 && exit.number == arch::GETPPID;
        OTHER.store(if again { 3 } else { 102 }, Ordering::Release);
    }
    let _ = sys(nr::THREAD_EXIT, [0; 6]);
    loop {
        core::hint::spin_loop();
    }
}

/// Wait for the other thread to reach `want`.
fn other_at(given: &Given, want: u64, case: i32) -> Result<(), i32> {
    for _ in 0..200_000 {
        let seen = OTHER.load(Ordering::Acquire);
        if seen == want {
            return Ok(());
        }
        if seen >= 100 {
            return Err(case);
        }
        nap(given)?;
    }
    Err(case)
}

/// Cases 168 to 175: the other thread on armed calls while this one makes its own, then blocks and
/// rejoins, reading what a join could race. Trips and declines both above zero where there are
/// two processors, or the case proved nothing (ledger 6998, X9). Neither is pinned: a thread with
/// a narrower mask keeps `PLACE` up and takes no fast path. On one processor the two share it,
/// and each stub's answer is held to its own thread across the switches (6787 W6).
fn two_threads(given: &Given) -> Result<(), i32> {
    OTHER.store(0, Ordering::Release);
    ask(given, ASK_WINDOW_LIVE, 0, None)?;
    let thread = sys(
        nr::THREAD_CREATE,
        [
            (other as *const ()).addr() as u64,
            top(&OTHER_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 168)?;
    other_at(given, 1, 168)?;
    ask(given, ASK_WINDOW_START, 0, None)?;

    for _ in 0..200 {
        let mut context = at(arch::armed_loop());
        arch::set_armed_loop(&mut context, 2_000, MINE);
        let mut exit = enter(&context);
        while exit.reason == Reason::Syscall as u64 {
            exit = resume_armed(MINE, arch::GETPPID);
        }
        fault_exit(&exit, arch::VECTOR_BREAKPOINT, 169)?;
        nap(given)?;
    }
    ask(given, ASK_WINDOW_TRIPS, SOME, None).map_err(|_| 170)?;

    // Two: this thread's joins race the other's stubs.
    if asked(given, super::ASK_TWO_PROCESSORS, 0, None)? {
        ask(given, ASK_WINDOW_START, 0, None)?;
        let mut held = 0_u64;
        for _ in 0..ROUNDS {
            nap(given)?;
            // In this space's restricted root, on a processor just joined: nothing of a stub.
            for (address, case) in [(STUB, 171), (DATA, 172), (CANARY, 173)] {
                let mut load = at(arch::load());
                arch::set_first(&mut load, address);
                let exit = enter(&load);
                fault_exit(&exit, arch::VECTOR_PAGE, case)?;
            }
            // A reader with no bit up, while this thread is blocked and the other runs stubs:
            // what the master root holds while open is no software's (ledger 6998, X1).
            ask(given, ASK_WINDOW_READERS, 0, None).map_err(|_| 178)?;
            // Each way a caller can aim at it.
            let at_data = u64::from(ferrix_native_abi::types::MAP_READ);
            let buffer = (&raw mut held).addr() as u64;
            hold(
                sys(nr::SPACE_READ, [given.space, DATA, buffer, 8, 0, 0]).is_err(),
                174,
            )?;
            hold(
                sys(nr::SPACE_WRITE, [given.space, STUB, buffer, 8, 0, 0]).is_err(),
                174,
            )?;
            hold(
                sys(
                    nr::SPACE_PROTECT,
                    [given.space, DATA, 0x1000, at_data, 0, 0],
                )
                .is_err(),
                174,
            )?;
            hold(
                sys(nr::SPACE_UNMAP, [given.space, DATA, 0x1000, 0, 0, 0]).is_err(),
                174,
            )?;
            hold(
                sys(
                    nr::SPACE_ALLOCATE,
                    [given.space, CANARY, 0x1000, at_data, 0, 0],
                )
                .is_err(),
                174,
            )?;
        }
        ask(given, ASK_WINDOW_BOTH, 0, None).map_err(|_| 175)?;
    }

    // Case 176: the kick drops the arm, and the next call exits as a call (6998 X6).
    sys(nr::RESTRICTED_KICK, [thread, 0, 0, 0, 0, 0]).map_err(|_| 176)?;
    other_at(given, 3, 176)?;
    // Case 177: its window goes with it, once it has ended and its handle is gone (6787 W11).
    let mut observed = 0_u32;
    sys(
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
    .map_err(|_| 177)?;
    sys(nr::HANDLE_CLOSE, [thread, 0, 0, 0, 0, 0]).map_err(|_| 177)?;
    ask(given, ASK_WINDOW_WENT, 0, None).map_err(|_| 177)
}

/// Every case. Ends with this thread free to run anywhere.
pub(super) fn all(given: &Given) -> Result<(), i32> {
    trips(given)?;
    vectors()?;
    closed()?;
    other_number()?;
    two_threads(given)
}
