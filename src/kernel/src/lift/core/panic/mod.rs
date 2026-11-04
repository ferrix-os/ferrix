use crate::arch;
use crate::console;
use crate::console::println;
pub use crate::fatal;
use crate::smp;
use core::panic::PanicInfo;
use core::ptr;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering;

/// A linuxd fatal: a Linux-layer panic is a process fault (M5-LIFT.md §3).
#[macro_export]
macro_rules! fatal {
    ($entry:expr, $($message:tt)+) => {{
        let _ = &$entry;
        panic!($($message)+)
    }};
}

pub use crate::panic::catalog::Explanation;
/// A Linux-layer panic is a process fault: the thread ends here.
pub fn end_thread() -> ! {
    crate::sched::exit_leaving()
}
