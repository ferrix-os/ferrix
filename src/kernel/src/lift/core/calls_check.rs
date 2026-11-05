//! Refusals of linuxd's own native calls, asked from linuxd's normal mode, the only
//! mode they answer in (`docs/microkernel/M5-LIFT.md` §10, ledger 5714 (15)).
//! `test-linuxd` reads its line. What a program in restricted mode
//! is refused is the row `restricted-check`'s table gives each of them.

use ferrix_native::call::Syscall as _;
use ferrix_native::error::Error;
use ferrix_native::lift_calls;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::lift::{CLOCK_COUNTER_HZ, KIND_VMO};
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_rt::Kernel;

/// A number no table holds.
const NOBODY: Handle = Handle(0x7fff_fff0);

fn expect<T: core::fmt::Debug>(
    what: &'static str,
    got: Result<T, Error>,
    want: Error,
) -> Result<(), &'static str> {
    match got {
        Err(error) if error == want => Ok(()),
        other => {
            crate::println!("  lift-calls {what}: {other:?}, not {want:?}");
            Err(what)
        }
    }
}

/// Each refusal, on `job`, a handle of the wrong kind for every call here, and
/// `arena`, the writable arena's. The number that held, or the first that did not.
pub fn run(job: Handle, arena: Handle, executable: Handle) -> Result<u32, &'static str> {
    let mask = [0_u8; ferrix_sched::WORDS * 8];
    let wrong_words = [0_u8; (ferrix_sched::WORDS + 1) * 8];
    expect(
        "DEBUG_WRITE, no such handle",
        lift_calls::debug_write(Kernel, NOBODY, b"x"),
        Error::BadHandle,
    )?;
    expect(
        "DEBUG_WRITE, a job",
        lift_calls::debug_write(Kernel, job, b"x"),
        Error::WrongType,
    )?;
    expect(
        "OBJECT_KIND, no such handle",
        lift_calls::object_kind(Kernel, NOBODY),
        Error::BadHandle,
    )?;
    expect(
        "CLOCK_READ, no such clock",
        lift_calls::clock_read(Kernel, 99),
        Error::InvalidArgs,
    )?;
    expect(
        "THREAD_AFFINITY, a job",
        lift_calls::thread_affinity(Kernel, job.0, &mask),
        Error::WrongType,
    )?;
    expect(
        "THREAD_AFFINITY, too many words",
        lift_calls::thread_affinity(Kernel, 0, &wrong_words),
        Error::InvalidArgs,
    )?;
    expect(
        "THREAD_AFFINITY, no processor",
        lift_calls::thread_affinity(Kernel, 0, &mask),
        Error::InvalidArgs,
    )?;
    expect(
        "BOOTSTRAP_MOVE, a job for a process",
        lift_calls::bootstrap_move(Kernel, job, NOBODY, false),
        Error::WrongType,
    )?;
    // The arena: a VMO, which can be neither copied nor handed on (ledger 6012, R5).
    let (kind, rights) =
        lift_calls::object_kind(Kernel, arena).map_err(|_| "OBJECT_KIND, the arena")?;
    if kind != KIND_VMO || rights & (Rights::DUPLICATE.0 | Rights::TRANSFER.0) != 0 {
        crate::println!("  lift-calls the arena: kind {kind}, rights {rights:#x}");
        return Err("the arena keeps a right to be copied or moved");
    }
    let arena_view =
        core::mem::ManuallyDrop::new(ferrix_native::handle::OwnedHandle::from_raw(Kernel, arena));
    expect(
        "the arena's duplicate",
        arena_view.duplicate(Requested::Same).map(|copy| copy.raw()),
        Error::AccessDenied,
    )?;
    expect(
        "the arena to a private table",
        ferrix_native::region::handle_to_private(Kernel, arena, Requested::Same),
        Error::AccessDenied,
    )?;
    // Asked of a kernel whose clock is the counter: a frequency, and it ticks.
    let _ = lift_calls::clock_read(Kernel, CLOCK_COUNTER_HZ)
        .map_err(|_| "CLOCK_READ, the frequency")?;
    // L.object.315: the three questions EL0 cannot answer itself, and any other refused.
    let hwcaps = lift_calls::clock_read(Kernel, ferrix_native_abi::lift::CLOCK_HWCAP)
        .map_err(|_| "CLOCK_READ, the hwcaps")?;
    // Every AArch64 core has fp and asimd; x86-64's answer is the kernel's own, unasked here.
    if cfg!(not(target_arch = "x86_64")) && hwcaps == 0 {
        return Err("CLOCK_READ answered no hwcaps");
    }
    let _ = lift_calls::clock_read(Kernel, ferrix_native_abi::lift::CLOCK_HWCAP2)
        .map_err(|_| "CLOCK_READ, the second hwcaps")?;
    let processor = lift_calls::clock_read(Kernel, ferrix_native_abi::lift::CLOCK_CPU)
        .map_err(|_| "CLOCK_READ, the processor")?;
    if processor as usize >= crate::smp::count() {
        return Err("CLOCK_READ answered a processor past the last");
    }
    expect(
        "CLOCK_READ with an unknown question",
        lift_calls::clock_read(Kernel, 1_000),
        Error::InvalidArgs,
    )?;
    window(job, arena, executable)?;
    Ok(19)
}

/// `SPACE_WINDOW` and `SPACE_PLACE`'s refusals (ledger 6411, W5, W6), on a process
/// made for the purpose and never started.
fn window(job: Handle, arena: Handle, executable: Handle) -> Result<(), &'static str> {
    use ferrix_native_abi::lift::{PLACE_EXECUTE, PLACE_MOST, PLACE_WRITE};
    let (_process, space) =
        crate::sys::process_new(b"lift-check").map_err(|()| "a process for the window's check")?;
    let space = Handle(space);
    let page: u64 = 0x10_0000;
    let entry = |flags: u64, offset: u64| {
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&(page | flags).to_le_bytes());
        bytes[8..].copy_from_slice(&offset.to_le_bytes());
        bytes
    };
    expect(
        "SPACE_WINDOW, a job",
        lift_calls::space_window(Kernel, job),
        Error::WrongType,
    )?;
    expect(
        "SPACE_PLACE, written and run",
        lift_calls::space_place(
            Kernel,
            space,
            executable,
            &entry(PLACE_WRITE | PLACE_EXECUTE, 0),
        ),
        Error::AccessDenied,
    )?;
    expect(
        "SPACE_PLACE, run through a handle that may not",
        lift_calls::space_place(Kernel, space, arena, &entry(PLACE_EXECUTE, 0)),
        Error::AccessDenied,
    )?;
    expect(
        "SPACE_PLACE, past the object",
        lift_calls::space_place(Kernel, space, arena, &entry(0, 1 << 40)),
        Error::InvalidArgs,
    )?;
    let many = [0_u8; (PLACE_MOST + 1) * 16];
    expect(
        "SPACE_PLACE, too many",
        lift_calls::space_place(Kernel, space, arena, &many),
        Error::InvalidArgs,
    )?;
    let view = core::mem::ManuallyDrop::new(ferrix_native::space::Space::from_owned(
        ferrix_native::handle::OwnedHandle::from_raw(Kernel, space),
    ));
    let writable = core::mem::ManuallyDrop::new(ferrix_native::vmo::Vmo::from_owned(
        ferrix_native::handle::OwnedHandle::from_raw(Kernel, arena),
    ));
    expect(
        "SPACE_MAP on a window",
        view.map(
            &writable,
            Some(page as usize),
            4096,
            ferrix_native::space::Access::Read,
            0,
        ),
        Error::BadState,
    )?;
    Ok(())
}
