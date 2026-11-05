use crate::arch;
use crate::arch::UserContext;
use crate::arch::UserRegs;
use crate::console::println;
use core::ptr;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_linux_abi::errno::Errno;
use ferrix_sync::Once;
pub fn handled_fault_count() -> u64 {
    0
}
#[derive(Debug, Clone, Copy)]
pub struct SyscallArgs {
    pub abi: Abi,
    pub number: usize,
    pub args: [u64; 6],
    pub ip: u64,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Abi {
    #[default]
    Native,
    Compat,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Return(isize),
    ReturnWords { value: isize, words: [u64; 3] },
    Enter { entry: u64, stack: u64, abi: Abi },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    Errno(u32),
    Trap,
}
#[derive(Clone, Copy, Debug)]
pub enum Fast {
    Declined,
    Tail(Outcome),
    Done(Outcome),
}
pub type FastWriteRead = fn(&[u64; 6]) -> Fast;
pub fn fast_write_read() -> Option<FastWriteRead> {
    None
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trap {
    PageFault(PageFault),
    Breakpoint,
    IllegalInstruction,
    Interrupt(u32),
    SystemCall,
    Fault { name: &'static str, code: u64 },
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageFault {
    pub address: u64,
    pub write: bool,
    pub execute: bool,
    pub present: bool,
    pub user: bool,
}
pub fn return_path() -> Option<&'static ReturnPath> {
    let raw = RETURN_PATH.load(core::sync::atomic::Ordering::Acquire);
    // SAFETY: only `set_return_path` writes it, from a `&'static`.
    unsafe { raw.as_ref() }
}
static RETURN_PATH: core::sync::atomic::AtomicPtr<ReturnPath> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static SYSCALL_FILTER: ferrix_sync::Once<SyscallFilter> = ferrix_sync::Once::new();
pub type SyscallFilter = fn(&SyscallArgs) -> Verdict;
/// As the kernel's entry asks it, on the same terms.
pub fn filter_system_call(args: &SyscallArgs) -> Option<Outcome> {
    let filter = SYSCALL_FILTER.get()?;
    match filter(args) {
        Verdict::Continue => None,
        Verdict::Errno(errno) => Some(Outcome::Return(-(errno.min(4095) as isize))),
        // As the entry's rollback: the result register keeps the number.
        Verdict::Trap => Some(Outcome::Return(args.number as isize)),
    }
}
pub fn set_quiet_syscall_filter(filter: SyscallFilter) {
    let _ = SYSCALL_FILTER.call_once(|| filter);
}
pub fn system_call(args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome {
    crate::restricted::dispatched(args, regs)
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultOutcome {
    Delivered,
    Ended { pid: u32 },
    NoProcess,
}
#[derive(Debug)]
pub struct ReturnPath {
    pub needs_attention: fn() -> bool,
    pub return_to_user: fn(&mut arch::UserContext),
    pub sigreturn: fn(&mut arch::UserContext, bool),
    pub fault_signal: fn(u32, i32, u64, u64) -> FaultOutcome,
    pub fault_end: fn(u32, i32, u64, u64) -> FaultOutcome,
}
pub fn set_return_path(path: &'static ReturnPath) {
    RETURN_PATH.store(
        core::ptr::from_ref(path).cast_mut(),
        core::sync::atomic::Ordering::Release,
    );
}
pub fn attention_due(path: &ReturnPath) -> bool {
    (path.needs_attention)()
}
