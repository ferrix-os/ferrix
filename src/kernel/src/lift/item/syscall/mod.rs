pub mod native;
use crate::arch;
use crate::console::println;
use crate::sched;
pub use crate::trap::Outcome;
pub use crate::trap::SyscallArgs;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;
pub use ferrix_kernel_core::syscall::uaccess;
use ferrix_linux_abi::errno;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
pub fn unanswered(call: Option<Syscall>, number: usize) {
    println!("linuxd: no answer for system call {number} ({call:?})");
}
pub trait Personality {
    fn answer(call: Syscall, args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome;
}

/// As the item's: the native range is the kernel's own, and reaches it without
/// passing here from a program; a number here is a Linux number.
pub fn dispatch_with<P: Personality>(args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome {
    // A native call whose row says it exits: linuxd's own to answer.
    if args.abi == crate::trap::Abi::Native
        && let Some(answer) = crate::cores::answer_native(args)
    {
        return Outcome::Return(answer);
    }
    let decoded = match args.abi {
        crate::trap::Abi::Native => arch::decode_syscall(args.number),
        crate::trap::Abi::Compat => arch::decode_compat_syscall(args.number),
    };
    let Some(call) = decoded else {
        unanswered(None, args.number);
        return Outcome::Return(Errno::ENOSYS.as_return_value());
    };
    P::answer(call, args, regs)
}

pub fn report_unanswered(lines: u32) {}
