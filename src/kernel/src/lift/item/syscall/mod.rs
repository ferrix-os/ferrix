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

/// The Linux table as an array, made once: `arch::decode_syscall` asks five `match`es in
/// turn, each a branch or two cold on every call.
static DECODED: ferrix_sync::Once<alloc::vec::Vec<Option<Syscall>>> = ferrix_sync::Once::new();

#[inline(always)]
fn decoded_native(number: usize) -> Option<Syscall> {
    let table = DECODED.call_once(|| (0..512).map(arch::decode_syscall).collect());
    match table.get(number) {
        Some(found) => *found,
        None => arch::decode_syscall(number),
    }
}

/// As the item's: the native range is the kernel's own, and reaches it without
/// passing here from a program; a number here is a Linux number.
pub fn dispatch_with<P: Personality>(args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome {
    // A native call whose row says it exits: linuxd's own to answer.
    // By range first: a lock and a scan of the handlers on every Linux call otherwise.
    if args.abi == crate::trap::Abi::Native
        && ferrix_native_abi::nr::is_native(args.number)
        && let Some(answer) = crate::cores::answer_native(args)
    {
        return Outcome::Return(answer);
    }
    let decoded = match args.abi {
        crate::trap::Abi::Native => decoded_native(args.number),
        crate::trap::Abi::Compat => arch::decode_compat_syscall(args.number),
    };
    let Some(call) = decoded else {
        unanswered(None, args.number);
        return Outcome::Return(Errno::ENOSYS.as_return_value());
    };
    P::answer(call, args, regs)
}

pub fn report_unanswered(lines: u32) {}
