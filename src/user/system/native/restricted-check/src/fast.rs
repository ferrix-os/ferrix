//! K1's fast path's own cases (`docs/microkernel/K1.md` §7.1, ledger 6093, 6464 Z4, 6747 (2b),
//! 7216 V.1-V.7), which only an image built `restricted_fastpath=on` runs: the kernel's check
//! says whether it has one, and each case reads how many trips went the fast way, so none
//! holds for having gone the long way. On every other image they take no time at a boot.
//!
//! Cases 133-134 (a thread killed with the mark up) and 144 (a barrier per exit over fast
//! trips) stand in `cases.rs`, as they hold on every image.

use core::sync::atomic::Ordering::{Acquire, Release};
use core::sync::atomic::{AtomicU64, Ordering};

use super::super::{
    ANY_PROCESSOR, ASK_PIN, ASK_TWO_PROCESSORS, Given, Reason, Stack, arch, ask, asked, at,
    continue_, enter, fault_exit, fill, hold, kick_racing, nr, read, resume, sys, syscall_exit,
    top,
};

/// Whether this image has the fast path: no other image answers yes.
const ASK_FAST_BUILT: u8 = 40;
/// Read this thread's counts.
const ASK_FAST_START: u8 = 41;
/// Whether count `word >> 56` of this thread moved by the rest of the word.
const ASK_FAST_MOVED: u8 = 42;
/// Normal mode's `FS` base in this thread's record: the word.
const ASK_NORMAL_FS: u8 = 43;
/// Its `GS` base.
const ASK_NORMAL_GS: u8 = 44;

/// Read the count of direct switches.
const ASK_DIRECT_START: u8 = 45;
/// Whether it moved by the word: yes on an image without them.
const ASK_DIRECT_MOVED: u8 = 46;

/// [`ASK_FAST_MOVED`]'s count of fast exits.
const EXITS: u64 = 0;
/// Of fast re-entries.
const RESUMES: u64 = 1;
/// Of `#NM`s taken with the mark up.
const LAZY: u64 = 2;

/// The program's `FS` and `GS` page, in the restricted space.
const PROGRAM_PAGE: u64 = 0x1500_0000;
/// What it holds: [`fill`]'s byte, eight times.
const PROGRAM_WORD: u64 = 0x5A5A_5A5A_5A5A_5A5A;
/// A page cases unmap under a loop of fast trips.
const SHOOTDOWN_PAGE: u64 = 0x0300_0000_0000;
/// How many round trips a case runs: a timer's tick may take any one the long way.
const ROUNDS: u64 = 20;
/// How many of them must have gone the fast way.
const FAST_AT_LEAST: u64 = 10;
/// Blocks of a millisecond a case waits for a thread before it says so: long past
/// anything a loaded gate does, and still a sentence rather than a hang.
const PATIENCE: u32 = 20_000;
/// XMM5's low word in the vector image, whose start is `FXSAVE`'s layout.
const XMM5_AT: usize = 240;

/// What normal mode's `FS` reads when its base is [`NORMAL_WORD`]'s address.
static NORMAL_WORD: u64 = 0x4E4F_524D_414C_0001;
/// The other thread's.
static OTHER_WORD: u64 = 0x4F54_4845_5200_0002;

/// This thread's handle, for an ask that carries it.
fn me(case: i32) -> Result<u64, i32> {
    sys(nr::THREAD_SELF, [0; 6]).map_err(|_| case)
}

/// Start counting this thread's trips.
fn count_from(given: &Given, case: i32) -> Result<(), i32> {
    ask(given, ASK_FAST_START, 0, Some(me(case)?)).map_err(|_| case)
}

/// That `which` moved by at least `least` since [`count_from`].
fn counted(given: &Given, which: u64, least: u64, case: i32) -> Result<(), i32> {
    hold(
        asked(
            given,
            ASK_FAST_MOVED,
            (which << 56) | least,
            Some(me(case)?),
        )?,
        case,
    )
}

/// Off this processor and back: the switch back writes the records to the processor.
fn block_a_moment(case: i32) -> Result<(), i32> {
    let now = sys(nr::CLOCK_READ, [0; 6]).map_err(|_| case)?;
    let deadline = now + 1_000_000;
    let port = sys(nr::PORT_CREATE, [0; 6]).map_err(|_| case)?;
    let mut observed = 0_u32;
    let _ = sys(
        nr::OBJECT_WAIT_ONE,
        [
            port,
            u64::from(ferrix_native_abi::signals::Signals::READABLE.0),
            (&raw const deadline).addr() as u64,
            (&raw mut observed).addr() as u64,
            0,
            0,
        ],
    );
    sys(nr::HANDLE_CLOSE, [port, 0, 0, 0, 0, 0])
        .map(|_| ())
        .map_err(|_| case)
}

/// Normal mode's base for this thread, on the processor when this returns.
fn normal_base(given: &Given, ask_what: u8, base: u64, case: i32) -> Result<(), i32> {
    ask(given, ask_what, base, Some(me(case)?)).map_err(|_| case)?;
    block_a_moment(case)
}

/// The processor this thread is on: a hint, read just after a call.
fn processor(case: i32) -> Result<u64, i32> {
    sys(nr::CLOCK_READ, [5, 0, 0, 0, 0, 0]).map_err(|_| case)
}

/// Move this thread to `cpu`, and leave its mask wide: a pinned thread has `PLACE` up for
/// good, and never goes the fast way (`sched::work`).
fn settle_on(given: &Given, cpu: u64, case: i32) -> Result<(), i32> {
    ask(given, ASK_PIN, cpu, Some(me(case)?)).map_err(|_| case)?;
    block_a_moment(case)?;
    ask(given, ASK_PIN, ANY_PROCESSOR, Some(me(case)?)).map_err(|_| case)
}

/// A context of `image` on [`PROGRAM_PAGE`]'s `FS`.
fn on_program_fs(image: u64) -> ferrix_native_abi::restricted::Context {
    let mut context = at(image);
    context.fs_base = PROGRAM_PAGE;
    context
}

/// Case 170: normal mode's base zero: the program's `FS` across fast trips (6093 (4)).
fn fs_with_normal_zero(given: &Given) -> Result<(), i32> {
    count_from(given, 170)?;
    for _ in 0..ROUNDS {
        syscall_exit(&enter(&on_program_fs(arch::fs_trip())), arch::GETPPID, 170)?;
        fault_exit(&resume(1), arch::VECTOR_BREAKPOINT, 170)?;
        let back = read()?;
        hold(back.rbx == PROGRAM_WORD && back.r12 == PROGRAM_WORD, 170)?;
    }
    counted(given, EXITS, FAST_AT_LEAST, 170)?;
    counted(given, RESUMES, FAST_AT_LEAST, 170)
}

/// Case 171: normal mode's base not zero: each mode reads its own `FS`, in the gap and in
/// the program, with the two controls of 6464 Z4 (no write at the exit, no write at the entry).
fn fs_with_normal_set(given: &Given) -> Result<(), i32> {
    normal_base(
        given,
        ASK_NORMAL_FS,
        (&raw const NORMAL_WORD).addr() as u64,
        171,
    )?;
    hold(arch::fs_word() == NORMAL_WORD, 171)?;
    count_from(given, 171)?;
    for _ in 0..ROUNDS {
        syscall_exit(&enter(&on_program_fs(arch::fs_trip())), arch::GETPPID, 171)?;
        hold(arch::fs_word() == NORMAL_WORD, 171)?;
        fault_exit(&resume(1), arch::VECTOR_BREAKPOINT, 171)?;
        let back = read()?;
        hold(back.rbx == PROGRAM_WORD && back.r12 == PROGRAM_WORD, 171)?;
        hold(arch::fs_word() == NORMAL_WORD, 171)?;
    }
    counted(given, EXITS, FAST_AT_LEAST, 171)?;
    counted(given, RESUMES, FAST_AT_LEAST, 171)?;
    normal_base(given, ASK_NORMAL_FS, 0, 171)
}

/// Case 172: `GS`: normal mode's own, with records that differ and with records that are
/// equal after a program's null load, which clears the base on some parts (ledger 7216 G.1).
fn gs_both_ways(given: &Given) -> Result<(), i32> {
    normal_base(
        given,
        ASK_NORMAL_GS,
        (&raw const NORMAL_WORD).addr() as u64,
        172,
    )?;
    hold(arch::gs_word() == NORMAL_WORD, 172)?;
    count_from(given, 172)?;
    for _ in 0..ROUNDS {
        let mut differ = at(arch::gs_trip());
        differ.gs_base = PROGRAM_PAGE;
        syscall_exit(&enter(&differ), arch::GETPPID, 182)?;
        hold(arch::gs_word() == NORMAL_WORD, 182)?;
        fault_exit(&resume(1), arch::VECTOR_BREAKPOINT, 183)?;
        let back = read()?;
        hold(back.rbx == PROGRAM_WORD && back.r12 == PROGRAM_WORD, 183)?;
    }
    counted(given, EXITS, FAST_AT_LEAST, 172)?;
    counted(given, RESUMES, FAST_AT_LEAST, 172)?;
    // The program's base is in the restricted window, so normal mode's reads the page there.
    normal_base(given, ASK_NORMAL_GS, PROGRAM_PAGE, 184)?;
    hold(arch::gs_word() == PROGRAM_WORD, 184)?;
    count_from(given, 184)?;
    for _ in 0..ROUNDS {
        let mut equal = at(arch::gs_null());
        equal.gs_base = PROGRAM_PAGE;
        syscall_exit(&enter(&equal), arch::GETPPID, 184)?;
        hold(arch::gs_word() == PROGRAM_WORD, 184)?;
        fault_exit(&resume(1), arch::VECTOR_BREAKPOINT, 185)?;
    }
    counted(given, EXITS, FAST_AT_LEAST, 184)?;
    normal_base(given, ASK_NORMAL_GS, 0, 172)
}

/// Case 173: a program's null `FS` load across a fast trip: the hardware's stays, which on
/// this processor either keeps the base or has cleared it, and a general trip writes the
/// record back. So a program sees its base until a fast trip's next load on the first
/// kind of part, a fault at address zero on the second, and its record's base after any
/// general trip: the difference K1 §7.1 (4) accepts (ledger 6464 Z4).
fn null_fs_load() -> Result<(), i32> {
    syscall_exit(&enter(&on_program_fs(arch::fs_null())), arch::GETPPID, 173)?;
    let exit = resume(1);
    let kept = exit.reason == Reason::Syscall as u64;
    if kept {
        hold(read()?.rbx == PROGRAM_WORD, 173)?;
    }
    // The general way back in writes the record.
    block_a_moment(173)?;
    if !kept {
        syscall_exit(&continue_(), arch::GETPPID, 173)?;
        hold(read()?.rbx == PROGRAM_WORD, 173)?;
    }
    fault_exit(&resume(1), arch::VECTOR_BREAKPOINT, 173)?;
    hold(read()?.r12 == PROGRAM_WORD, 173)
}

/// Case 174's channel to the check, for the other thread.
static CHANNEL: AtomicU64 = AtomicU64::new(0);
/// Its steps: 1 the other thread is ready, 2 it may look, 3 it found its own.
static STEP: AtomicU64 = AtomicU64::new(0);
/// Its stack.
static OTHER_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// What the other thread keeps in `XMM5`.
const OTHER_VECTOR: u64 = 0xB0B0_B0B0_0000_0174;

/// [`CHANNEL`] as the part of [`Given`] an ask reads.
fn channel_only() -> Given {
    Given {
        channel: CHANNEL.load(Ordering::Relaxed),
        region: 0,
        job: 0,
        space: 0,
        region_space: 0,
        start_answers: ferrix_linux_abi::errno::Errno::ENOSYS,
    }
}

/// The next task on the processor when case 174's thread blocks in the gap.
fn other_thread() -> Result<(), i32> {
    let given = channel_only();
    // Pinned for good: `PLACE` is this thread's, and it never goes in.
    ask(&given, ASK_PIN, 0, Some(me(174)?)).map_err(|_| 174)?;
    normal_base(
        &given,
        ASK_NORMAL_FS,
        (&raw const OTHER_WORD).addr() as u64,
        174,
    )?;
    arch::clobber_vectors(OTHER_VECTOR);
    hold(arch::fs_word() == OTHER_WORD, 174)?;
    STEP.store(1, Release);
    // A yield keeps vector registers, as a wait does not (`vectors_die_in`). The other
    // thread is blocked or in the gap; its own patience bounds this.
    while STEP.load(Acquire) != 2 {
        sys(nr::THREAD_YIELD, [0; 6]).map_err(|_| 174)?;
    }
    hold(arch::fs_word() == OTHER_WORD, 174)?;
    hold(arch::read_vectors() == OTHER_VECTOR, 174)
}

/// [`other_thread`] as a thread's body.
extern "C" fn other_body(_argument: usize) -> ! {
    STEP.store(if other_thread().is_ok() { 3 } else { 100 }, Release);
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// Wait for [`STEP`] to reach `want`, blocking, by the thread on this processor's own.
fn await_step(want: u64, case: i32) -> Result<(), i32> {
    let mut left = PATIENCE;
    loop {
        match STEP.load(Acquire) {
            state if state == want => return Ok(()),
            state if state >= 100 => return Err(case),
            _ => {}
        }
        block_a_moment(case)?;
        left = left.checked_sub(1).ok_or(case)?;
    }
}

/// Case 174: blocked in the gap: the next task finds its own `FS` and its own vector
/// registers, and the program finds its own by the general way back (6464 Z4, 6093 (1)).
fn blocked_in_the_gap(given: &Given) -> Result<(), i32> {
    sys(
        nr::THREAD_CREATE,
        [
            (other_body as *const ()).addr() as u64,
            top(&OTHER_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 174)?;
    await_step(1, 174)?;
    let mut context = on_program_fs(arch::fs_vectors());
    arch::set_first(&mut context, 0x0174_0174_0174_0174);
    // Off a pinned thread's processor is no case: the other thread stays on 0.
    let mut tries = 50;
    loop {
        settle_on(given, 0, 174)?;
        count_from(given, 174)?;
        syscall_exit(&enter(&context), arch::GETPPID, 174)?;
        if processor(174)? == 0 {
            break;
        }
        tries -= 1;
        hold(tries > 0, 190)?;
    }
    STEP.store(2, Release);
    await_step(3, 174)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 174)?;
    let back = read()?;
    hold(back.rbx == PROGRAM_WORD && back.r12 == PROGRAM_WORD, 174)?;
    hold(back.rax == 0x0174_0174_0174_0174, 174)?;
    counted(given, EXITS, 1, 174)
}

/// Case 175: `VECTORS_READ` in the gap, which takes the mark down, reads the program's; an
/// `ENTER` after it runs (6464 Z4). Case 176: a vector instruction in the gap takes `#NM`,
/// the program's bytes come back and normal mode's are never seen. Case 177:
/// `VECTORS_WRITE` in the gap, then `RESUME`, reads what was written (7216 V.6).
fn vectors_in_the_gap(given: &Given) -> Result<(), i32> {
    const FIRST: u64 = 0x1750_1750_1750_1750;
    const SECOND: u64 = 0x1751_1751_1751_1751;
    const THIRD: u64 = 0x1770_1770_1770_1770;
    const NORMAL: u64 = 0x1760_1760_1760_1760;
    let mut context = at(arch::fs_vectors());
    context.fs_base = PROGRAM_PAGE;
    arch::set_first(&mut context, FIRST);
    let mut image = [0_u8; ferrix_native_abi::restricted::VECTORS_BYTES];
    syscall_exit(&enter(&context), arch::GETPPID, 175)?;
    let bytes =
        ferrix_rt::native::region::vectors_read(ferrix_rt::Kernel, &mut image).map_err(|_| 175)?;
    hold(bytes == image.len(), 175)?;
    let word = image
        .get(XMM5_AT..XMM5_AT + 8)
        .and_then(|word| <[u8; 8]>::try_from(word).ok())
        .map(u64::from_ne_bytes);
    hold(word == Some(FIRST), 175)?;
    arch::set_first(&mut context, SECOND);
    syscall_exit(&enter(&context), arch::GETPPID, 175)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 175)?;
    hold(read()?.rax == SECOND, 175)?;

    count_from(given, 176)?;
    arch::clobber_vectors(0);
    arch::set_first(&mut context, FIRST);
    syscall_exit(&enter(&context), arch::GETPPID, 176)?;
    arch::clobber_vectors(NORMAL);
    hold(arch::read_vectors() == NORMAL, 176)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 176)?;
    hold(read()?.rax == FIRST, 176)?;
    hold(arch::read_vectors() == NORMAL, 176)?;
    counted(given, LAZY, 1, 176)?;

    syscall_exit(&enter(&context), arch::GETPPID, 177)?;
    let _ =
        ferrix_rt::native::region::vectors_read(ferrix_rt::Kernel, &mut image).map_err(|_| 177)?;
    image
        .get_mut(XMM5_AT..XMM5_AT + 8)
        .ok_or(177)?
        .copy_from_slice(&THIRD.to_ne_bytes());
    ferrix_rt::native::region::vectors_write(ferrix_rt::Kernel, &image).map_err(|_| 177)?;
    fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 177)?;
    hold(read()?.rax == THIRD, 177)
}

/// Case 178's unmapping thread's stack.
static UNMAPPER_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// The processor it unmapped from.
static UNMAPPED_ON: AtomicU64 = AtomicU64::new(0);
/// The restricted space, for it.
static SPACE: AtomicU64 = AtomicU64::new(0);

/// Pinned to processor 1: unmap the page on the word, under case 178's loop.
extern "C" fn unmapper(_argument: usize) -> ! {
    let given = channel_only();
    let settled = settle_on(&given, 1, 178);
    STEP.store(if settled.is_ok() { 11 } else { 100 }, Release);
    while STEP.load(Acquire) == 11 {
        core::hint::spin_loop();
    }
    let on = processor(178).unwrap_or(u64::MAX);
    UNMAPPED_ON.store(on, Release);
    let unmapped = sys(
        nr::SPACE_UNMAP,
        [SPACE.load(Acquire), SHOOTDOWN_PAGE, 0x1000, 0, 0, 0],
    );
    STEP.store(if unmapped.is_ok() { 13 } else { 100 }, Release);
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// Case 178: a shootdown on two processors across fast trips only: a page unmapped under a
/// loop that does nothing but load it and go round is gone within the trip after, with no
/// general entry to help (6093 (6)).
fn shootdown_across_fast_trips(given: &Given) -> Result<(), i32> {
    if !asked(given, ASK_TWO_PROCESSORS, 0, None)? {
        return Ok(());
    }
    let write = u64::from(ferrix_native_abi::types::MAP_READ | ferrix_native_abi::types::MAP_WRITE);
    sys(
        nr::SPACE_ALLOCATE,
        [given.space, SHOOTDOWN_PAGE, 0x1000, write, 0, 0],
    )
    .map_err(|_| 178)?;
    SPACE.store(given.space, Release);
    STEP.store(10, Release);
    settle_on(given, 0, 178)?;
    sys(
        nr::THREAD_CREATE,
        [
            (unmapper as *const ()).addr() as u64,
            top(&UNMAPPER_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 178)?;
    await_step(11, 178)?;
    count_from(given, 178)?;
    let mut context = at(arch::load_trips());
    arch::set_first(&mut context, SHOOTDOWN_PAGE);
    let mut exit = enter(&context);
    let mut trips = 0_u64;
    let mut after = 0_u64;
    while exit.reason == Reason::Syscall as u64 {
        trips += 1;
        if trips == 1_000 {
            STEP.store(12, Release);
        }
        if STEP.load(Acquire) == 13 {
            after += 1;
            // At most the one in flight when the word was read, and the next.
            hold(after <= 2, 186)?;
        }
        hold(trips < 500_000_000, 187)?;
        exit = resume(1);
    }
    hold(exit.reason == Reason::Fault as u64, 188)?;
    hold(UNMAPPED_ON.load(Acquire) != processor(178)?, 191)?;
    counted(given, EXITS, 500, 189)?;
    counted(given, RESUMES, 500, 189)
}

/// How many times case 179 kicks: an interrupt in restricted mode ends the loop by itself,
/// so only the rounds where it lands in normal mode lean on the word's term.
const KICKS: u64 = 64;
/// Case 179's thread's stack.
static KICKED_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// The round it is in.
static KICK_ROUND: AtomicU64 = AtomicU64::new(0);
/// The round whose kick has returned.
static KICK_SENT: AtomicU64 = AtomicU64::new(0);
/// Trips a loop may make after that: the one in flight, the one the read raced, one for a
/// preemption between the read and the trip.
const TRIPS_PAST_A_KICK: u32 = 3;
/// Set when a round's exit was anything but a kick.
const KICK_WRONG: u64 = u64::MAX;

/// Pinned to processor 1: rounds of getppid and nothing else, each ended by a kick.
extern "C" fn kicked(_argument: usize) -> ! {
    let given = channel_only();
    if settle_on(&given, 1, 179).is_err() || count_from(&given, 179).is_err() {
        KICK_ROUND.store(KICK_WRONG, Release);
    }
    let mut round = 1;
    while round <= KICKS {
        let mut exit = enter(&at(arch::trips()));
        KICK_ROUND.store(round, Release);
        // From the kick's return, the look at the whole word stops this at the next crossing
        // or the one in flight: an interrupt alone would leave it for the next timer.
        let mut after = 0_u32;
        while exit.reason == Reason::Syscall as u64 {
            if KICK_SENT.load(Acquire) == round {
                after += 1;
                if after > TRIPS_PAST_A_KICK {
                    KICK_ROUND.store(KICK_WRONG, Release);
                }
            }
            exit = resume(1);
        }
        if exit.reason != Reason::Kick as u64 {
            KICK_ROUND.store(KICK_WRONG, Release);
        }
        round += 1;
    }
    // Each round's loop went round; a round may end before its first trip's way back.
    KICK_ROUND.store(
        if counted(&given, EXITS, KICKS / 2, 179).is_ok() {
            KICKS + 1
        } else {
            KICK_WRONG
        },
        Release,
    );
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// Case 179: a kick against a loop of fast trips only ends it, by the word as well as by
/// the interrupt, and a loop the word does not reach ends in this sentence rather than a
/// hang (6747 (2b)). Case 180: the same for a job's kill.
fn ended_in_a_loop_of_trips(given: &Given) -> Result<(), i32> {
    if !asked(given, ASK_TWO_PROCESSORS, 0, None)? {
        return Ok(());
    }
    settle_on(given, 0, 179)?;
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
    .map_err(|_| 179)?;
    let mut wanted = 1;
    while wanted <= KICKS {
        let mut left = PATIENCE;
        while KICK_ROUND.load(Acquire) < wanted {
            hold(KICK_ROUND.load(Acquire) != KICK_WRONG, 179)?;
            block_a_moment(179)?;
            left = left.checked_sub(1).ok_or(179)?;
        }
        hold(KICK_ROUND.load(Acquire) != KICK_WRONG, 179)?;
        kick_racing(thread, 179)?;
        KICK_SENT.store(wanted, Release);
        wanted += 1;
    }
    let mut left = PATIENCE;
    while KICK_ROUND.load(Acquire) != KICKS + 1 {
        hold(KICK_ROUND.load(Acquire) != KICK_WRONG, 179)?;
        block_a_moment(179)?;
        left = left.checked_sub(1).ok_or(179)?;
    }
    for _ in 0..8 {
        killed_in_a_loop_of_trips(given)?;
    }
    Ok(())
}

/// Case 180's body: getppid and nothing else.
extern "C" fn trips_only(_argument: usize) -> ! {
    STEP.store(20, Release);
    let mut exit = enter(&at(arch::trips()));
    while exit.reason == Reason::Syscall as u64 {
        exit = resume(1);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Case 180's stack: one at a time.
static KILLED_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));

/// One round of case 180.
fn killed_in_a_loop_of_trips(given: &Given) -> Result<(), i32> {
    let child = sys(nr::JOB_CREATE, [given.job, 0, 0, 0, 0, 0]).map_err(|_| 180)?;
    let mut out = 0_u64;
    let name = b"trips";
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
    .map_err(|_| 180)?;
    super::super::map_space(out).map_err(|_| 180)?;
    STEP.store(19, Release);
    sys(
        nr::PROCESS_START_SHARED,
        [
            process,
            (trips_only as *const ()).addr() as u64,
            top(&KILLED_STACK),
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 180)?;
    await_step(20, 180)?;
    // Let it go round for a while.
    block_a_moment(180)?;
    sys(nr::JOB_KILL, [child, 0, 0, 0, 0, 0]).map_err(|_| 180)?;
    let now = sys(nr::CLOCK_READ, [0; 6]).map_err(|_| 180)?;
    let deadline = now + u64::from(PATIENCE) * 1_000_000;
    let mut observed = 0_u32;
    sys(
        nr::OBJECT_WAIT_ONE,
        [
            process,
            u64::from(ferrix_native_abi::signals::Signals::TERMINATED.0),
            (&raw const deadline).addr() as u64,
            (&raw mut observed).addr() as u64,
            0,
            0,
        ],
    )
    .map_err(|_| 180)?;
    sys(nr::HANDLE_CLOSE, [process, 0, 0, 0, 0, 0]).map_err(|_| 180)?;
    sys(nr::HANDLE_CLOSE, [child, 0, 0, 0, 0, 0])
        .map(|_| ())
        .map_err(|_| 180)
}

/// The server's end, for case 181.
static SERVER_END: AtomicU64 = AtomicU64::new(0);
/// Its stack.
static SERVER_STACK: Stack = Stack(core::cell::UnsafeCell::new([0; 0x8000]));
/// What it keeps in `XMM5` across its wait, whose vector registers die across the call
/// but for what it reads back as it asks for them.
const SERVER_WORD: u64 = 0x1810_1810_1810_1810;
/// One round's word from the client.
const CLIENT_WORD: u64 = 0x1811_1811_1811_1811;

/// The next task on the processor when case 181's client calls in the gap: a server that
/// answers from a word the kernel's switch gave it, and with `CR0.TS` clear for it.
extern "C" fn server(_argument: usize) -> ! {
    let given = channel_only();
    let settled = settle_on(&given, 0, 181);
    let end = SERVER_END.load(Acquire);
    STEP.store(if settled.is_ok() { 31 } else { 100 }, Release);
    let mut count = nr::WRITE_READ_NOTHING as u64;
    let mut word = 0_u64;
    loop {
        let answer = arch::channel_write_read(end, count, word);
        let Ok((_, received)) = answer else {
            break;
        };
        // A vector instruction, after a direct switch from a thread whose mark was up:
        // it must not trap in a way this task can see, and it must read its own.
        arch::clobber_vectors(SERVER_WORD);
        if arch::read_vectors() != SERVER_WORD || received != CLIENT_WORD {
            STEP.store(100, Release);
        }
        word = received + 1;
        count = 8;
    }
    let _ = sys(nr::THREAD_EXIT, [0, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

/// Case 181: a direct switch away from a normal-mode thread with the mark up, under
/// `ipc_fastpath=on` when the image has it and by the general path otherwise: the server
/// finds `CR0.TS` clear and its own vectors, and the program finds its own (7216 V.7).
fn channel_call_in_the_gap(given: &Given) -> Result<(), i32> {
    let mut ends = [0_u32; 2];
    sys(
        nr::CHANNEL_CREATE,
        [ends.as_mut_ptr().addr() as u64, 0, 0, 0, 0, 0],
    )
    .map_err(|_| 181)?;
    SERVER_END.store(u64::from(ends[1]), Release);
    STEP.store(30, Release);
    settle_on(given, 0, 181)?;
    sys(
        nr::THREAD_CREATE,
        [
            (server as *const ()).addr() as u64,
            top(&SERVER_STACK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| 181)?;
    await_step(31, 181)?;
    // Let it get into its wait, so that the call finds a waiter.
    block_a_moment(181)?;
    let mut context = at(arch::fs_vectors());
    context.fs_base = PROGRAM_PAGE;
    count_from(given, 181)?;
    ask(given, ASK_DIRECT_START, 0, None).map_err(|_| 181)?;
    for round in 0..ROUNDS {
        let program = 0x1812_0000_0000_0000 | round;
        arch::set_first(&mut context, program);
        syscall_exit(&enter(&context), arch::GETPPID, 181)?;
        let (bytes, answer) =
            arch::channel_write_read(u64::from(ends[0]), 8, CLIENT_WORD).map_err(|()| 181)?;
        hold(bytes == 8 && answer == CLIENT_WORD + 1, 181)?;
        hold(STEP.load(Acquire) != 100, 181)?;
        fault_exit(&resume(0), arch::VECTOR_BREAKPOINT, 181)?;
        hold(read()?.rax == program, 181)?;
    }
    sys(nr::HANDLE_CLOSE, [u64::from(ends[0]), 0, 0, 0, 0, 0]).map_err(|_| 181)?;
    counted(given, EXITS, FAST_AT_LEAST, 181)?;
    ask(given, ASK_DIRECT_MOVED, FAST_AT_LEAST, None).map_err(|_| 192)
}

/// Every case, on an image that has the fast path.
pub(in super::super) fn run(given: &Given) -> Result<(), i32> {
    if !asked(given, ASK_FAST_BUILT, 0, None)? {
        return Ok(());
    }
    CHANNEL.store(given.channel, Ordering::Relaxed);
    fill(given.space, PROGRAM_PAGE, 0x5A, 170)?;
    fs_with_normal_zero(given)?;
    fs_with_normal_set(given)?;
    gs_both_ways(given)?;
    null_fs_load()?;
    if arch::has_vectors() {
        blocked_in_the_gap(given)?;
        vectors_in_the_gap(given)?;
        channel_call_in_the_gap(given)?;
    }
    shootdown_across_fast_trips(given)?;
    ended_in_a_loop_of_trips(given)
}
