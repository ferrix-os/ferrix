//! `reboot`'s `RESTART2`: a restart with a word for the firmware, which on a
//! DK board's U-Boot is the boot mode it comes back up in (the arm moved from
//! `syscall/system.rs`'s `sys_reboot`; `docs/KERNEL-CRATES.md` §6.2). Called
//! after the call's privilege check and its magic numbers, it does in
//! today's order what `sys_reboot` did for it: the commits, the audit record,
//! then the word, the lines and the reset. A `fixed` build refuses the call
//! before the commits, so a refused call records nothing.

use alloc::vec::Vec;

use ferrix_bootinfo::pstore::Stop;
use ferrix_kernel_load::arch;
use ferrix_kernel_load::console::println;
use ferrix_kernel_load::syscall::process::Process;
use ferrix_kernel_load::syscall::uaccess;
use ferrix_linux_abi::errno::Errno;

/// `reboot(.., RESTART2, arg)`.
pub fn restart_with_word(process: &Process, arg: u64) -> Result<usize, Errno> {
    ferrix_kernel_load::power::sync_disks();
    let caller = ferrix_kernel_load::object::process::Host::core(process);
    ferrix_kernel_load::audit::power(
        ferrix_kernel_load::audit::Subject::of(caller),
        ferrix_audit::power::RESTART,
    );
    // Linux copies at most 255 bytes into a 256-byte buffer and cuts
    // a longer word off there; a word with no NUL in that reach is
    // EFAULT here instead, which no caller that means it will meet.
    let mut word = Vec::new();
    uaccess::copy_cstr_from_user(process.space(), arg, 255, &mut word)
        .map_err(|_| Errno::EFAULT)?;
    let word = core::str::from_utf8(&word).unwrap_or("");
    println!("reboot: Restarting system with command '{word}'");
    // Where the firmware reads a word, on the one machine that has
    // one: a DK board's U-Boot, whose support registers with power.
    match ferrix_kernel_load::power::request_boot_mode(word) {
        Ok(what) => println!("reboot: {what}"),
        Err(why) => println!("reboot: '{word}' changes nothing: {why}"),
    }
    let _ = ferrix_kernel_load::console::pstore::stop(Stop::Restart);
    arch::reset()
}
