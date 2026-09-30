//! The vDSO's boot check: the image says what it exports where the code is,
//! the data page says what the kernel's clock is, a program sees the pages it
//! should and may not write them, and a program's call through the vDSO
//! answers between two system calls.
//!
//! The last is the one that shows the arithmetic. A program calls
//! `clock_gettime` as a system call, then through the vDSO, then as a system
//! call again, for each of the seven clocks the vDSO answers, and the three
//! must be in order; then `gettimeofday` and `time` the same way. A vDSO
//! whose scaling or offset was the kernel's but for a factor or a sign
//! answers outside them. Under a hypervisor with an
//! invariant TSC that is the code reading the TSC; under an emulator, whose
//! clock is the HPET, it is the code falling back to the system call, which
//! is the other half.

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_vma::VmaFlags;

use crate::syscall::uaccess;
use crate::syscall::{exec, image, time, vdso};
use crate::user::space::{Access, AddressSpace};

/// What the program exits with when every call answered in order.
const PROGRAM_STATUS: i32 = 120;

/// Where the program calls each function, in bytes from the image's start:
/// the slots the architecture's code puts them in.
const SLOTS: [(&str, &str, usize); 3] = [
    (
        "__vdso_clock_gettime",
        "clock_gettime",
        ferrix_vdso::CODE_AT,
    ),
    (
        "__vdso_gettimeofday",
        "gettimeofday",
        ferrix_vdso::CODE_AT + 128,
    ),
    ("__vdso_time", "time", ferrix_vdso::CODE_AT + 256),
];

/// What the check found: which way the vDSO answers the clock.
pub(crate) type Answered = &'static str;

/// What the check answers for a vDSO that is only the signal return
/// trampoline, AArch64's: no clock was read through it.
pub(crate) const SIGRETURN_ONLY: Answered = "the signal return trampoline alone";

/// Run the check, or `Ok(None)` on an architecture without a vDSO.
///
/// An image with no clock -- AArch64's, which is `__kernel_rt_sigreturn`
/// alone -- is checked for exporting the trampoline where its code puts it,
/// its data page and its mapping, and answers [`SIGRETURN_ONLY`].
/// Verifies: `L.x86_64.101`
pub(crate) fn check_the_vdso() -> Result<Option<Answered>, &'static str> {
    let Some(image) = vdso::image() else {
        return Ok(None);
    };
    let has_clock = crate::arch::vdso_spec().is_some_and(|spec| {
        spec.functions
            .iter()
            .any(|function| function.name == SLOTS[0].0)
    });
    if !has_clock {
        check_the_trampoline(&image)?;
        let _ = check_the_data_page()?;
        check_the_mapping()?;
        return Ok(Some(SIGRETURN_ONLY));
    }
    check_the_image(&image)?;
    let answered = check_the_data_page()?;
    check_the_mapping()?;
    check_a_program_reads_the_clock_through_it()?;
    Ok(Some(answered))
}

/// The trampoline is exported, by both lookups, at the offset its code has,
/// and a space it is mapped into finds it there.
///
/// Verifies: L.user.77
fn check_the_trampoline(image: &[u8]) -> Result<(), &'static str> {
    let spec = crate::arch::vdso_spec().ok_or("the vDSO has no image to check")?;
    let function = spec
        .functions
        .iter()
        .find(|function| function.name == vdso::SIGRETURN)
        .ok_or("the vDSO has no signal return trampoline")?;
    let slot = ferrix_vdso::CODE_AT + function.offset;
    let at = ferrix_vdso::lookup(image, vdso::SIGRETURN, Some(ferrix_vdso::VERSION));
    let hashed = ferrix_vdso::lookup_hashed(image, vdso::SIGRETURN, Some(ferrix_vdso::VERSION));
    if at != Some(slot) || hashed != Some(slot) {
        return Err("the vDSO's image does not export its signal return trampoline where it is");
    }
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let mapped = vdso::map_into(&space, false).ok_or("the vDSO could not be mapped")?;
    if vdso::sigreturn(&space) != Some(mapped + slot as u64) {
        return Err("a space the vDSO is mapped into does not find its trampoline there");
    }
    Ok(())
}

/// Every function at `LINUX_2.6`, under its alias too, found by both
/// lookups at the slot the program calls it at.
fn check_the_image(image: &[u8]) -> Result<(), &'static str> {
    let at = |name| ferrix_vdso::lookup(image, name, Some(ferrix_vdso::VERSION));
    let hashed = |name| ferrix_vdso::lookup_hashed(image, name, Some(ferrix_vdso::VERSION));
    for (name, alias, slot) in SLOTS {
        if at(name) != Some(slot) || at(alias) != Some(slot) || hashed(name) != Some(slot) {
            return Err("the vDSO's image does not export each function where its code puts it");
        }
    }
    Ok(())
}

/// The data page names the kernel's counter, its frequency and the
/// real-time offset, and follows a change of the offset.
fn check_the_data_page() -> Result<Answered, &'static str> {
    let [mode, hz, offset] = vdso::data().ok_or("the vDSO's data page could not be read")?;
    let (want, answered) = if crate::arch::vdso_can_read_counter() {
        (ferrix_vdso::MODE_TSC, "reading the counter itself")
    } else {
        (ferrix_vdso::MODE_SYSCALL, "making the system call")
    };
    if mode != want {
        return Err("the vDSO's data page does not say which counter the kernel reads");
    }
    if hz != crate::timer::counter_hz() {
        return Err("the vDSO's data page does not hold the counter's frequency");
    }
    let was = time::realtime_offset();
    if offset != was.cast_unsigned() {
        return Err("the vDSO's data page does not hold the real-time clock's offset");
    }
    let moved = was.wrapping_add(1_000_000_007);
    time::restore_realtime_offset(moved);
    let followed = vdso::data().map(|[_, _, offset]| offset);
    time::restore_realtime_offset(was);
    let back = vdso::data().map(|[_, _, offset]| offset);
    if followed != Some(moved.cast_unsigned()) || back != Some(was.cast_unsigned()) {
        return Err("the vDSO's data page did not follow the real-time clock being set");
    }
    Ok(answered)
}

/// A space the vDSO is mapped into shows the image read-and-run and the data
/// page read-only below it, and refuses to write either or make either
/// writable.
///
/// Verifies: L.user.76
fn check_the_mapping() -> Result<(), &'static str> {
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let at = vdso::map_into(&space, false).ok_or("the vDSO could not be mapped")?;
    let data = at - PAGE_SIZE;
    let regions = space
        .regions()
        .map_err(|_| "could not list a space's regions")?;
    let flags_at = |start| {
        regions
            .iter()
            .find(|region| region.start == start)
            .map(|region| {
                (
                    region.flags.read,
                    region.flags.write,
                    region.flags.execute,
                    region.flags.shared,
                )
            })
    };
    if flags_at(at) != Some((true, false, true, true))
        || flags_at(data) != Some((true, false, false, true))
    {
        return Err("the vDSO is not mapped read-and-run over a read-only data page");
    }
    let mut magic = [0_u8; 4];
    uaccess::copy_from_user(&space, at, &mut magic)
        .map_err(|_| "the vDSO's image could not be read")?;
    if magic != [0x7f, b'E', b'L', b'F'] {
        return Err("the vDSO's mapping does not show its image");
    }
    for page in [data, at] {
        if space.with_page(page, Access::WRITE, |_| ()).is_ok() {
            return Err("a write to the vDSO's pages was let through");
        }
        if space.protect(page, PAGE_SIZE, VmaFlags::READ_WRITE).is_ok() {
            return Err("mprotect made the vDSO's pages writable");
        }
    }
    Ok(())
}

/// `USER_VDSO_PROGRAM`, run: every function held to its system call.
/// Verifies: `L.x86_64.41`, H.SCHED.9
fn check_a_program_reads_the_clock_through_it() -> Result<(), &'static str> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        crate::arch::ARCH.elf_machine(),
        image::Shape::Good,
        crate::arch::USER_VDSO_PROGRAM,
    );
    let status = exec::run(&file, &[b"/vdso"], &[], [0x5a; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a program that reads the clock through the vDSO could not be started")?;
    match status {
        PROGRAM_STATUS => Ok(()),
        2 => Err("a program was started without AT_SYSINFO_EHDR"),
        11..=74 => Err(match status % 10 {
            1 => "the vDSO's clock_gettime failed for a clock it answers",
            2 => "the vDSO's clock_gettime answered before a system call made ahead of it",
            3 => "the vDSO's clock_gettime answered after a system call made behind it",
            _ => "the vDSO's clock_gettime answered nanoseconds out of range",
        }),
        81 => Err("the vDSO's gettimeofday failed"),
        82 => Err("the vDSO's gettimeofday answered before a system call made ahead of it"),
        83 => Err("the vDSO's gettimeofday answered after a system call made behind it"),
        84 => Err("the vDSO's gettimeofday answered microseconds out of range"),
        91 => Err("the vDSO's time returned one time and stored another"),
        92 | 93 => Err("the vDSO's time answered outside the system calls on either side"),
        95 => Err("the vDSO's clock_gettime did not answer an unknown clock with -EINVAL"),
        98 => Err("a system call was refused to a program checking the vDSO"),
        _ => Err("a program that reads the clock through the vDSO did not exit with 120"),
    }
}
