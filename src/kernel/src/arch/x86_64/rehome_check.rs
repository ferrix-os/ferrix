//! What x86-64 says of an entry and an image, on values of the check's own: no
//! personality, no program. Its caller is `trap::rehome_check::run`, and it builds
//! in the core's crate (`docs/microkernel/B5.md`, class A).

use crate::arch;
use crate::trap::Abi;

/// x86-64's rollback value is the number, for both of its entries.
fn check_the_rollback() -> Result<(), &'static str> {
    let args = [0x1111, 2, 3, 4, 5, 6];
    for abi in [Abi::Native, Abi::Compat] {
        if arch::syscall_rollback_value(abi, 0x2222, &args) != 0x2222 {
            return Err("a rolled-back frame would not read the number in RAX");
        }
    }
    Ok(())
}

/// An image of another machine has no ABI, and the loader refuses it for that.
fn check_an_image_of_another_machine() -> Result<(), &'static str> {
    /// `EM_AARCH64`.
    const EM_AARCH64: u16 = 183;
    if arch::image_abi(ferrix_elf::Class::Elf64, EM_AARCH64).is_some() {
        return Err("image_abi answered an ABI for an image of another machine");
    }
    if arch::image_abi(ferrix_elf::Class::Elf64, arch::ARCH.elf_machine()) != Some(Abi::Native) {
        return Err("image_abi answered no ABI for an image of this machine");
    }
    Ok(())
}

/// `counter_is_tsc` answers true only when what `counter_now` reads is the TSC.
fn check_the_counter_is_named() -> Result<(), &'static str> {
    let reads_the_tsc = (0..8).any(|_| {
        let before = super::cpu::rdtsc();
        let now = arch::counter_now();
        let after = super::cpu::rdtsc();
        before <= now && now <= after
    });
    if super::clock::counter_is_tsc() != reads_the_tsc {
        return Err("counter_is_tsc does not say what counter_now reads");
    }
    if reads_the_tsc && super::clock::counter_hz() == 0 {
        return Err("a counter that is the TSC has no rate");
    }
    Ok(())
}

/// Run them, and answer how many cases they are.
///
/// # Errors
///
/// The first thing that was not so, by name.
pub(crate) fn run() -> Result<u64, &'static str> {
    check_the_rollback()?;
    check_an_image_of_another_machine()?;
    check_the_counter_is_named()?;
    Ok(3)
}
