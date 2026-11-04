pub mod input;
pub mod output;
pub mod pstore;
pub mod screen;
use core::fmt;
use core::fmt::Write;
use core::hint::spin_loop;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use ferrix_sync::IrqSpinLock;
pub fn write(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write as _;
    // One line, one write, so that threads' lines do not cross.
    let mut line = alloc::string::String::new();
    let _ = line.write_fmt(args);
    let bytes = line.as_bytes();
    crate::console::log::record(bytes, crate::console::log::KERNEL);
    crate::sys::debug_write(bytes);
}
pub fn write_bytes(bytes: &[u8]) {
    crate::console::log::record(bytes, crate::console::log::PROGRAM);
    crate::sys::debug_write(bytes);
}
pub fn write_raw(bytes: &[u8]) {
    crate::sys::debug_write(bytes);
}
pub fn write_record(priority: u8, line: &[u8]) {
    let _ = crate::console::log::break_line();
    crate::console::log::record(line, priority);
    crate::sys::debug_write(line);
}
pub use crate::println;

/// Print to the debug log, with a newline.
#[macro_export]
macro_rules! println {
    () => { $crate::console::write(format_args!("\n")) };
    ($($argument:tt)*) => {
        $crate::console::write(format_args!("{}\n", format_args!($($argument)*)))
    };
}
