//! Syscall User Dispatch as Proton's Wine uses it, run as init by `cargo
//! xtask test-sud` (`docs/SECCOMP.md` §13, the design's 7.2): through raw
//! `prctl(PR_SET_SYSCALL_USER_DISPATCH)` calls, a range that holds the
//! program's own code, a selector byte, and calls made from a page outside
//! it, at 0x600000000000 as DW-Proton's self-test maps it. Built for x86-64
//! and for i386, which the kernel runs in compatibility mode.
//!
//! Each step prints `sud: <step> ok`, and the program ends with `sud: all
//! ok` and status 0, or `sud: FAILED <step>: <what>` and status 1.
//!
//! * **setup**: a `SIGSYS` handler, the page and its stubs, and dispatch on
//!   for the text mapping that holds this program's code;
//! * **allow**: with the selector at ALLOW, a call from the page runs;
//! * **block**: at BLOCK it is dispatched: it does not run, `SIGSYS` comes
//!   with `si_code` 2, `si_errno` 0, the address after the instruction, the
//!   number and the entry's `AUDIT_ARCH`, and the call returns what the
//!   handler wrote into the context;
//! * **context**: what the handler finds in the context (the number in the
//!   result register, the instruction pointer after the call, and on x86-64
//!   `RCX` and `R11` as `SYSCALL` left them) and that what it writes there,
//!   `RCX` and `R11` included, is what the program resumes with;
//! * **special**: a number that reads as `-ERESTARTSYS` and, on x86-64, one in
//!   Ferrix's native range are dispatched like any other;
//! * **compat**: on x86-64 an `int $0x80` from the page is dispatched with
//!   i386's `AUDIT_ARCH`; in the i386 build the `siginfo` is at i386's
//!   offsets;
//! * **blocked**: with `SIGSYS` blocked, or ignored, a dispatch ends the
//!   process with `SIGSYS`;
//! * **kills**: a selector of 2 ends the process with `SIGSYS`, one that
//!   cannot be read with `SIGSEGV`;
//! * **prctl**: every `EINVAL` and `EFAULT` row, and `OFF`;
//! * **inherit**: a thread, a fork child and an exec'd program start with
//!   dispatch off;
//! * **seccomp**: with a filter that traps `getpid` and kills 0xffff,
//!   dispatch is judged first;
//! * **nt**: on x86-64, the NT numbers a Windows program's own code makes
//!   directly under Wine, from pages where an image sits high in the user
//!   half, each dispatched and resumed.
//!
//! Built with `negative-control-rax`, the handler leaves the result out;
//! built with `negative-control-allow`, the selector is never set to BLOCK.
//! Each must fail on the block step.

use std::io::Write;
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};

use libc::{c_int, c_long, c_ulong, c_void};

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

// ---------------------------------------------------------------------------
// The kernel's constants, from `linux/prctl.h`, `linux/audit.h` and
// `linux/seccomp.h`.
// ---------------------------------------------------------------------------

/// `PR_SET_SYSCALL_USER_DISPATCH`.
const PR_SET_SYSCALL_USER_DISPATCH: c_int = 59;
/// `PR_SYS_DISPATCH_OFF`.
const DISPATCH_OFF: c_ulong = 0;
/// `PR_SYS_DISPATCH_EXCLUSIVE_ON`.
const DISPATCH_EXCLUSIVE_ON: c_ulong = 1;
/// `PR_SYS_DISPATCH_INCLUSIVE_ON`.
const DISPATCH_INCLUSIVE_ON: c_ulong = 2;
/// `SYSCALL_DISPATCH_FILTER_ALLOW`.
const ALLOW: u8 = 0;
/// `SYSCALL_DISPATCH_FILTER_BLOCK`.
const BLOCK: u8 = 1;
/// `SYS_SECCOMP`, a filter's trap.
const SYS_SECCOMP: i32 = 1;
/// `SYS_USER_DISPATCH`, a dispatch.
const SYS_USER_DISPATCH: i32 = 2;
/// `AUDIT_ARCH_X86_64`.
const ARCH_X86_64: u32 = 0xC000_003E;
/// `AUDIT_ARCH_I386`.
const ARCH_I386: u32 = 0x4000_0003;
/// This build's own `AUDIT_ARCH`.
const OWN_ARCH: u32 = if cfg!(target_arch = "x86_64") {
    ARCH_X86_64
} else {
    ARCH_I386
};
/// `PR_SET_NO_NEW_PRIVS`.
const PR_SET_NO_NEW_PRIVS: c_int = 38;
/// `SECCOMP_SET_MODE_FILTER`.
const SET_MODE_FILTER: c_ulong = 1;
/// `SECCOMP_RET_TRAP` with its data.
const RET_TRAP: u32 = 0x0003_0000;
/// `SECCOMP_RET_KILL_PROCESS`.
const RET_KILL_PROCESS: u32 = 0x8000_0000;
/// `SECCOMP_RET_ALLOW`.
const RET_ALLOW: u32 = 0x7FFF_0000;

/// `getpid` on this build's ABI.
const GETPID: c_long = libc::SYS_getpid;
/// `getpid` on i386's, which an `int $0x80` from 64-bit code names.
#[cfg(target_arch = "x86_64")]
const GETPID_I386: u64 = 20;
/// A number no Linux ABI has, as DW-Proton's self-test uses it.
const UNKNOWN: u64 = 0xFFFF;
/// Ferrix's `channel_write_read`, in its native range.
#[cfg(target_arch = "x86_64")]
const NATIVE: u64 = 0x1013;
/// What the handler writes as a dispatched call's result.
const EMULATED: u64 = 0x1234;

/// Where the page of stubs goes: DW-Proton's address on x86-64, and one an
/// i386 program can name.
#[cfg(target_arch = "x86_64")]
const PAGE_AT: usize = 0x6000_0000_0000;
/// See the 64-bit [`PAGE_AT`].
#[cfg(target_arch = "x86")]
const PAGE_AT: usize = 0x6000_0000;
/// `MAP_FIXED_NOREPLACE`.
const MAP_FIXED_NOREPLACE: c_int = 0x10_0000;

/// An address in the kernel's half, which a selector may not name. The i386
/// build has none to name.
#[cfg(target_arch = "x86_64")]
const KERNEL_HALF: c_ulong = 0xFFFF_8000_0000_0000;
/// See the 64-bit [`KERNEL_HALF`]; never used as an address here.
#[cfg(target_arch = "x86")]
const KERNEL_HALF: c_ulong = 0;

/// The selector byte the kernel reads.
static SELECTOR: AtomicU8 = AtomicU8::new(ALLOW);

// ---------------------------------------------------------------------------
// The stubs, written into the page.
// ---------------------------------------------------------------------------

/// x86-64: `mov %rdi,%rax; syscall; mov %rcx,(%rsi); mov %r11,8(%rsi); ret`.
/// The address after `syscall` is the stub's plus 5.
#[cfg(target_arch = "x86_64")]
const STUB: &[u8] = &[
    0x48, 0x89, 0xF8, 0x0F, 0x05, 0x48, 0x89, 0x0E, 0x4C, 0x89, 0x5E, 0x08, 0xC3,
];
/// i386: `mov 4(%esp),%eax; int $0x80; ret`. The address after `int $0x80` is
/// the stub's plus 6.
#[cfg(target_arch = "x86")]
const STUB: &[u8] = &[0x8B, 0x44, 0x24, 0x04, 0xCD, 0x80, 0xC3];
/// Where the call's next instruction is in [`STUB`].
const AFTER: usize = if cfg!(target_arch = "x86_64") { 5 } else { 6 };
/// x86-64: `mov %rdi,%rax; int $0x80; ret`, at [`STUB80_AT`] in the page: a
/// 64-bit program's compat call. The address after `int $0x80` is plus 5.
#[cfg(target_arch = "x86_64")]
const STUB80: &[u8] = &[0x48, 0x89, 0xF8, 0xCD, 0x80, 0xC3];
/// Where [`STUB80`] goes in the page.
#[cfg(target_arch = "x86_64")]
const STUB80_AT: usize = 64;

/// A stub's signature: the number, and where the x86-64 stub stores `RCX`
/// and `R11` after the call.
type Stub = unsafe extern "C" fn(usize, *mut [u64; 2]) -> usize;

/// The page, once mapped.
static PAGE: AtomicU64 = AtomicU64::new(0);

/// Make a call from the page with `number`, and answer its result and, on
/// x86-64, what `RCX` and `R11` held after it.
fn from_page(number: u64) -> (u64, [u64; 2]) {
    let page = PAGE.load(Ordering::Acquire) as usize;
    let mut after = [0_u64; 2];
    // SAFETY: the page holds `STUB` at its start, mapped executable, and the
    // stub keeps the C calling convention.
    let stub: Stub = unsafe { std::mem::transmute::<usize, Stub>(page) };
    // SAFETY: as above; `after` is alive for the call.
    let answer = unsafe { stub(number as usize, &raw mut after) };
    (answer as u64, after)
}

/// The same through [`STUB80`].
#[cfg(target_arch = "x86_64")]
fn from_page_int80(number: u64) -> u64 {
    let page = PAGE.load(Ordering::Acquire) as usize;
    let mut after = [0_u64; 2];
    // SAFETY: the page holds `STUB80` at `STUB80_AT`, mapped executable.
    let stub: Stub = unsafe { std::mem::transmute::<usize, Stub>(page + STUB80_AT) };
    // SAFETY: as above.
    let answer = unsafe { stub(number as usize, &raw mut after) };
    answer as u64
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// Print one line at once, so the serial log has it before the next step.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The last error's number.
fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `prctl(PR_SET_SYSCALL_USER_DISPATCH, mode, offset, len, selector)`.
fn dispatch(mode: c_ulong, offset: c_ulong, len: c_ulong, selector: c_ulong) -> (c_int, c_int) {
    // SAFETY: the option takes plain integers and an address the kernel
    // checks.
    let answer = unsafe { libc::prctl(PR_SET_SYSCALL_USER_DISPATCH, mode, offset, len, selector) };
    (answer, errno())
}

/// This program's own text: the mapping that holds [`main`].
fn own_text() -> Result<(u64, u64), String> {
    let here = main as *const () as u64;
    let maps = std::fs::read_to_string("/proc/self/maps")
        .map_err(|error| format!("/proc/self/maps: {error}"))?;
    for line in maps.lines() {
        let Some((range, _)) = line.split_once(' ') else {
            continue;
        };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        if (start..end).contains(&here) {
            return Ok((start, end - start));
        }
    }
    Err("no mapping of /proc/self/maps holds main".into())
}

/// Turn dispatch on for this thread: calls from this program's text run, and
/// the selector judges the rest.
fn dispatch_on() -> Step {
    let (start, len) = own_text()?;
    match dispatch(
        DISPATCH_EXCLUSIVE_ON,
        start as c_ulong,
        len as c_ulong,
        SELECTOR.as_ptr() as c_ulong,
    ) {
        (0, _) => Ok(()),
        (answer, error) => Err(format!(
            "PR_SET_SYSCALL_USER_DISPATCH answered {answer}, errno {error}"
        )),
    }
}

/// Set the selector.
fn select(value: u8) {
    if cfg!(feature = "negative-control-allow") && value == BLOCK {
        return;
    }
    SELECTOR.store(value, Ordering::SeqCst);
}

/// Fork, running `child` in the child, which ends with its answer.
fn fork_with(child: impl FnOnce() -> c_int) -> Result<libc::pid_t, String> {
    // SAFETY: the child calls only libc functions and `_exit`s.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork failed, errno {}", errno()));
    }
    if pid == 0 {
        let status = child();
        // SAFETY: ends the child without running the parent's destructors.
        unsafe { libc::_exit(status) };
    }
    Ok(pid)
}

/// Wait for `pid` and return its exit status, or 1000 plus the signal that
/// ended it.
fn reap(pid: libc::pid_t) -> c_int {
    let mut status = 0;
    // SAFETY: status is a live int.
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1000 + libc::WTERMSIG(status)
    }
}

/// What a child that ends by `SIGSYS` reports.
const BY_SIGSYS: c_int = 1000 + libc::SIGSYS;
/// What a child that ends by `SIGSEGV` reports.
const BY_SIGSEGV: c_int = 1000 + libc::SIGSEGV;

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

/// Signals the handler took.
static TAKEN: AtomicU32 = AtomicU32::new(0);
/// The last one's `si_code`, `si_errno`, `_call_addr`, `_syscall`, `_arch`.
static CODE: AtomicI32 = AtomicI32::new(0);
/// See [`CODE`].
static ERRNO: AtomicI32 = AtomicI32::new(0);
/// See [`CODE`].
static CALL_ADDR: AtomicU64 = AtomicU64::new(0);
/// See [`CODE`].
static SYSCALL: AtomicI32 = AtomicI32::new(0);
/// See [`CODE`].
static ARCH: AtomicU32 = AtomicU32::new(0);
/// The context as the handler found it: the result register, the
/// instruction pointer, and on x86-64 `RCX`, `R11` and the flags.
static FOUND_RESULT: AtomicU64 = AtomicU64::new(0);
/// See [`FOUND_RESULT`].
static FOUND_IP: AtomicU64 = AtomicU64::new(0);
/// See [`FOUND_RESULT`].
static FOUND_RCX: AtomicU64 = AtomicU64::new(0);
/// See [`FOUND_RESULT`].
static FOUND_R11: AtomicU64 = AtomicU64::new(0);
/// See [`FOUND_RESULT`].
static FOUND_FLAGS: AtomicU64 = AtomicU64::new(0);

/// What the x86-64 handler writes into `RCX` and `R11`.
const WRITTEN_RCX: u64 = 0x5678;
/// See [`WRITTEN_RCX`].
const WRITTEN_R11: u64 = 0x202;

/// i386's `gregs` indices: `ECX`, `EAX`, `EIP`.
#[cfg(target_arch = "x86")]
const I386_EAX: usize = 11;
/// See [`I386_EAX`].
#[cfg(target_arch = "x86")]
const I386_EIP: usize = 14;

/// Register `index` of an i386 context.
#[cfg(target_arch = "x86")]
fn i386_register(context: &libc::ucontext_t, index: usize) -> u32 {
    let base = std::ptr::from_ref(&context.uc_mcontext).cast::<u32>();
    // SAFETY: `gregs` is nineteen words and `index` is below that.
    unsafe { base.add(index).read() }
}

/// Write register `index` of an i386 context.
#[cfg(target_arch = "x86")]
fn i386_set_register(context: &mut libc::ucontext_t, index: usize, value: u32) {
    let base = std::ptr::from_mut(&mut context.uc_mcontext).cast::<u32>();
    // SAFETY: as above.
    unsafe { base.add(index).write(value) }
}

/// Read the context, and write the emulated result and, on x86-64, `RCX` and
/// `R11`, as Wine's handler writes its frame.
fn emulate(context: &mut libc::ucontext_t) {
    #[cfg(target_arch = "x86_64")]
    {
        let reg = |index: c_int| {
            context
                .uc_mcontext
                .gregs
                .get(index as usize)
                .copied()
                .unwrap_or(0) as u64
        };
        FOUND_RESULT.store(reg(libc::REG_RAX), Ordering::Release);
        FOUND_IP.store(reg(libc::REG_RIP), Ordering::Release);
        FOUND_RCX.store(reg(libc::REG_RCX), Ordering::Release);
        FOUND_R11.store(reg(libc::REG_R11), Ordering::Release);
        FOUND_FLAGS.store(reg(libc::REG_EFL), Ordering::Release);
        let mut put = |index: c_int, value: u64| {
            if let Some(slot) = context.uc_mcontext.gregs.get_mut(index as usize) {
                *slot = value as _;
            }
        };
        put(libc::REG_RCX, WRITTEN_RCX);
        put(libc::REG_R11, WRITTEN_R11);
        if !cfg!(feature = "negative-control-rax") {
            put(libc::REG_RAX, EMULATED);
        }
    }
    #[cfg(target_arch = "x86")]
    {
        FOUND_RESULT.store(
            u64::from(i386_register(context, I386_EAX)),
            Ordering::Release,
        );
        FOUND_IP.store(
            u64::from(i386_register(context, I386_EIP)),
            Ordering::Release,
        );
        if !cfg!(feature = "negative-control-rax") {
            i386_set_register(context, I386_EAX, EMULATED as u32);
        }
    }
}

/// `SIGSYS`'s handler: record what the signal and the context say, then
/// write the result, as Wine's does.
extern "C" fn on_sigsys(_signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
    if info.is_null() || context.is_null() {
        return;
    }
    // SAFETY: the kernel passes a `siginfo_t` and a `ucontext_t` that are
    // alive for the handler.
    let (info, context) = unsafe { (&*info, &mut *context.cast::<libc::ucontext_t>()) };
    let word = size_of::<usize>();
    // `siginfo_t`: three `int`s, then the union at the first pointer-aligned
    // offset; `_sigsys` is a pointer and two `int`s.
    let union = 12_usize.div_ceil(word) * word;
    let base = std::ptr::from_ref(info).cast::<u8>();
    // SAFETY: the offsets are inside `siginfo_t`, which is 128 bytes.
    let (call_addr, syscall, arch) = unsafe {
        (
            base.add(union).cast::<usize>().read_unaligned() as u64,
            base.add(union + word).cast::<i32>().read_unaligned(),
            base.add(union + word + 4).cast::<u32>().read_unaligned(),
        )
    };
    CODE.store(info.si_code, Ordering::Release);
    ERRNO.store(info.si_errno, Ordering::Release);
    CALL_ADDR.store(call_addr, Ordering::Release);
    SYSCALL.store(syscall, Ordering::Release);
    ARCH.store(arch, Ordering::Release);
    emulate(context);
    let _ = TAKEN.fetch_add(1, Ordering::AcqRel);
}

/// Install the handler for `SIGSYS`.
fn install_handler() -> Step {
    // SAFETY: a zeroed `sigaction` with the fields set is a valid argument.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_sigsys as *const () as usize;
    action.sa_flags = libc::SA_SIGINFO;
    // SAFETY: `action` is alive for the call.
    let answer = unsafe { libc::sigaction(libc::SIGSYS, &action, std::ptr::null_mut()) };
    if answer != 0 {
        return Err(format!("sigaction(SIGSYS) failed, errno {}", errno()));
    }
    Ok(())
}

/// The signal one dispatched call must have raised, its number `number`,
/// from `after` with `arch`.
fn expect_dispatch(before: u32, number: u64, after: u64, arch: u32) -> Step {
    if TAKEN.load(Ordering::Acquire) != before + 1 {
        return Err("a call from outside the range under BLOCK was not dispatched".into());
    }
    let (code, errno, call_addr, syscall, found_arch) = (
        CODE.load(Ordering::Acquire),
        ERRNO.load(Ordering::Acquire),
        CALL_ADDR.load(Ordering::Acquire),
        SYSCALL.load(Ordering::Acquire),
        ARCH.load(Ordering::Acquire),
    );
    if cfg!(target_arch = "x86") && (call_addr != after || syscall != number as u32 as i32) {
        return Err(format!(
            "compat siginfo: _call_addr {call_addr:#x} and _syscall {syscall:#x} are not at \
             i386's offsets 12 and 16 (want {after:#x}, {number:#x})"
        ));
    }
    if code != SYS_USER_DISPATCH
        || errno != 0
        || call_addr != after
        || syscall != number as u32 as i32
        || found_arch != arch
    {
        return Err(format!(
            "the SIGSYS said si_code {code}, si_errno {errno}, _call_addr {call_addr:#x}, \
             _syscall {syscall:#x}, _arch {found_arch:#x}; want 2, 0, {after:#x}, \
             {number:#x}, {arch:#x}"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The steps
// ---------------------------------------------------------------------------

/// **setup**.
fn setup() -> Step {
    install_handler()?;
    // SAFETY: an anonymous mapping at a fixed address that must be free.
    let page = unsafe {
        libc::mmap(
            PAGE_AT as *mut c_void,
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if page != PAGE_AT as *mut c_void {
        return Err(format!(
            "mmap at {PAGE_AT:#x} gave {page:?}, errno {}",
            errno()
        ));
    }
    // SAFETY: the page is ours, writable and 4096 bytes long.
    unsafe {
        std::ptr::copy_nonoverlapping(STUB.as_ptr(), page.cast::<u8>(), STUB.len());
        #[cfg(target_arch = "x86_64")]
        std::ptr::copy_nonoverlapping(
            STUB80.as_ptr(),
            page.cast::<u8>().add(STUB80_AT),
            STUB80.len(),
        );
    }
    // SAFETY: the page is ours.
    if unsafe { libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
        return Err(format!("mprotect of the page failed, errno {}", errno()));
    }
    PAGE.store(page as u64, Ordering::Release);
    dispatch_on()
}

/// **allow**.
fn allow() -> Step {
    select(ALLOW);
    let before = TAKEN.load(Ordering::Acquire);
    let (answer, _) = from_page(GETPID as u64);
    let pid = std::process::id() as u64;
    if TAKEN.load(Ordering::Acquire) != before {
        return Err("an ALLOW selector dispatched a call".into());
    }
    if answer != pid {
        return Err(format!(
            "getpid from the page answered {answer:#x}, want {pid:#x}"
        ));
    }
    Ok(())
}

/// **block**.
fn block() -> Step {
    let page = PAGE.load(Ordering::Acquire);
    select(BLOCK);
    let before = TAKEN.load(Ordering::Acquire);
    let (answer, _) = from_page(UNKNOWN);
    select(ALLOW);
    expect_dispatch(before, UNKNOWN, page + AFTER as u64, OWN_ARCH)?;
    if answer & 0xFFFF_FFFF != EMULATED {
        return Err(format!(
            "a dispatched call did not return the result its handler wrote: {answer:#x}"
        ));
    }
    Ok(())
}

/// **context**.
fn context() -> Step {
    let page = PAGE.load(Ordering::Acquire);
    let after = page + AFTER as u64;
    select(BLOCK);
    let (answer, kept) = from_page(UNKNOWN);
    select(ALLOW);
    let found_result = FOUND_RESULT.load(Ordering::Acquire);
    let found_ip = FOUND_IP.load(Ordering::Acquire);
    if found_result & 0xFFFF_FFFF != UNKNOWN || found_ip != after {
        return Err(format!(
            "the handler found the result register {found_result:#x} and the ip \
             {found_ip:#x}; want the number {UNKNOWN:#x} and {after:#x}"
        ));
    }
    if answer & 0xFFFF_FFFF != EMULATED {
        return Err(format!(
            "the program resumed with {answer:#x}, not the handler's result"
        ));
    }
    if cfg!(target_arch = "x86_64") {
        let (rcx, r11, flags) = (
            FOUND_RCX.load(Ordering::Acquire),
            FOUND_R11.load(Ordering::Acquire),
            FOUND_FLAGS.load(Ordering::Acquire),
        );
        if rcx != after || r11 != flags {
            return Err(format!(
                "the handler found RCX {rcx:#x} and R11 {r11:#x} with the flags {flags:#x}; \
                 want RCX = {after:#x} and R11 = the flags, as SYSCALL left them"
            ));
        }
        if kept != [WRITTEN_RCX, WRITTEN_R11] {
            return Err(format!(
                "the program resumed with RCX {:#x} and R11 {:#x}, not the {WRITTEN_RCX:#x} \
                 and {WRITTEN_R11:#x} its handler wrote",
                kept[0], kept[1]
            ));
        }
    }
    Ok(())
}

/// **special**.
fn special() -> Step {
    let page = PAGE.load(Ordering::Acquire);
    let after = page + AFTER as u64;
    let restart = (-512_i64) as u64;
    select(BLOCK);
    let before = TAKEN.load(Ordering::Acquire);
    let (answer, _) = from_page(restart);
    select(ALLOW);
    expect_dispatch(before, restart, after, OWN_ARCH)?;
    if answer & 0xFFFF_FFFF != EMULATED {
        return Err(format!(
            "a dispatched -ERESTARTSYS was restarted or ran: {answer:#x}"
        ));
    }
    #[cfg(target_arch = "x86_64")]
    {
        select(BLOCK);
        let before = TAKEN.load(Ordering::Acquire);
        let (answer, _) = from_page(NATIVE);
        select(ALLOW);
        expect_dispatch(before, NATIVE, after, OWN_ARCH)?;
        if answer != EMULATED {
            return Err(format!("a dispatched native-range call ran: {answer:#x}"));
        }
    }
    Ok(())
}

/// **compat**.
fn compat() -> Step {
    #[cfg(target_arch = "x86_64")]
    {
        let page = PAGE.load(Ordering::Acquire);
        select(BLOCK);
        let before = TAKEN.load(Ordering::Acquire);
        let answer = from_page_int80(GETPID_I386);
        select(ALLOW);
        expect_dispatch(before, GETPID_I386, page + STUB80_AT as u64 + 5, ARCH_I386)?;
        if answer & 0xFFFF_FFFF != EMULATED {
            return Err(format!(
                "a dispatched int $0x80 did not return its handler's result: {answer:#x}"
            ));
        }
        // With ALLOW it runs, as i386's getpid.
        let answer = from_page_int80(GETPID_I386);
        if answer != u64::from(std::process::id()) {
            return Err(format!("an allowed int $0x80 getpid answered {answer:#x}"));
        }
    }
    #[cfg(target_arch = "x86")]
    {
        // `block` read the fields at i386's offsets; read them once more
        // here, by name, for a getpid.
        let page = PAGE.load(Ordering::Acquire);
        select(BLOCK);
        let before = TAKEN.load(Ordering::Acquire);
        let (_, _) = from_page(GETPID as u64);
        select(ALLOW);
        expect_dispatch(before, GETPID as u64, page + AFTER as u64, ARCH_I386)?;
    }
    Ok(())
}

/// **blocked**: `SIGSYS` blocked, then ignored: a dispatch is fatal.
fn blocked() -> Step {
    for ignore in [false, true] {
        let child = fork_with(|| {
            if dispatch_on().is_err() {
                return 2;
            }
            // SAFETY: plain signal-set and disposition calls on our own
            // process.
            unsafe {
                if ignore {
                    let _ = libc::signal(libc::SIGSYS, libc::SIG_IGN);
                } else {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    let _ = libc::sigemptyset(&mut set);
                    let _ = libc::sigaddset(&mut set, libc::SIGSYS);
                    let _ = libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                }
            }
            SELECTOR.store(BLOCK, Ordering::SeqCst);
            let _ = from_page(UNKNOWN);
            3
        })?;
        match reap(child) {
            BY_SIGSYS => {}
            other => {
                return Err(format!(
                    "a dispatch with SIGSYS {} ended its process with {other}, not SIGSYS",
                    if ignore { "ignored" } else { "blocked" }
                ));
            }
        }
    }
    Ok(())
}

/// **kills**: a selector of 2, and one that cannot be read.
fn kills() -> Step {
    let child = fork_with(|| {
        if dispatch_on().is_err() {
            return 2;
        }
        SELECTOR.store(2, Ordering::SeqCst);
        let _ = from_page(GETPID as u64);
        3
    })?;
    match reap(child) {
        BY_SIGSYS => {}
        other => {
            return Err(format!(
                "a selector of 2 ended its process with {other}, not SIGSYS"
            ));
        }
    }
    let child = fork_with(|| {
        // SAFETY: an anonymous page of our own, unmapped below.
        let byte = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if byte == libc::MAP_FAILED {
            return 4;
        }
        let Ok((start, len)) = own_text() else {
            return 2;
        };
        if dispatch(
            DISPATCH_EXCLUSIVE_ON,
            start as c_ulong,
            len as c_ulong,
            byte as c_ulong,
        )
        .0 != 0
        {
            return 2;
        }
        // SAFETY: the page is ours.
        let _ = unsafe { libc::munmap(byte, 4096) };
        let _ = from_page(GETPID as u64);
        3
    })?;
    match reap(child) {
        BY_SIGSEGV => Ok(()),
        other => Err(format!(
            "a selector that cannot be read ended its process with {other}, not SIGSEGV"
        )),
    }
}

/// **prctl**: every refusal, in a child so that this thread's settings stay.
fn prctl_rows() -> Step {
    let child = fork_with(|| {
        // A range that wraps is refused only where the arguments are 64
        // bits wide: an i386 caller's arrive zero-extended on the 64-bit
        // kernel, as on Linux, so 0xffff_fffe + 4 does not wrap there. The
        // i386 build asks an empty inclusive range twice instead.
        let wraps: (c_ulong, c_ulong, c_ulong, c_ulong, c_int) = if cfg!(target_arch = "x86_64") {
            (DISPATCH_EXCLUSIVE_ON, c_ulong::MAX - 1, 4, 0, libc::EINVAL)
        } else {
            (DISPATCH_INCLUSIVE_ON, 0x2000, 0, 0, libc::EINVAL)
        };
        // A kernel-half selector is EFAULT. An i386 program cannot name one:
        // every 32-bit address is in the 64-bit kernel's user half, as on
        // Linux, whose access_ok checks the 64-bit limit. The i386 build asks
        // a mode that is none instead.
        let kernel_half: (c_ulong, c_ulong, c_ulong, c_ulong, c_int) =
            if cfg!(target_arch = "x86_64") {
                (
                    DISPATCH_EXCLUSIVE_ON,
                    0x1000,
                    0x1000,
                    KERNEL_HALF,
                    libc::EFAULT,
                )
            } else {
                (4, 0, 0, 0, libc::EINVAL)
            };
        let rows: [(c_ulong, c_ulong, c_ulong, c_ulong, c_int); 8] = [
            (DISPATCH_OFF, 1, 0, 0, libc::EINVAL),
            (DISPATCH_OFF, 0, 1, 0, libc::EINVAL),
            (
                DISPATCH_OFF,
                0,
                0,
                SELECTOR.as_ptr() as c_ulong,
                libc::EINVAL,
            ),
            (DISPATCH_EXCLUSIVE_ON, 0x1000, 0, 0, libc::EINVAL),
            wraps,
            (DISPATCH_INCLUSIVE_ON, 0x1000, 0, 0, libc::EINVAL),
            (3, 0, 0, 0, libc::EINVAL),
            kernel_half,
        ];
        for (row, (mode, offset, len, selector, want)) in rows.into_iter().enumerate() {
            if dispatch(mode, offset, len, selector) != (-1, want) {
                return 10 + row as c_int;
            }
        }
        // Offset 0 with any length is accepted. With no selector it would
        // dispatch every call, this child's next prctl and its handler's
        // rt_sigreturn included, so it is given the selector, at ALLOW.
        SELECTOR.store(ALLOW, Ordering::SeqCst);
        if dispatch(DISPATCH_EXCLUSIVE_ON, 0, 0, SELECTOR.as_ptr() as c_ulong).0 != 0 {
            return 3;
        }
        if dispatch(DISPATCH_OFF, 0, 0, 0).0 != 0 {
            return 4;
        }
        // Off: a call from the page with BLOCK runs.
        SELECTOR.store(BLOCK, Ordering::SeqCst);
        if from_page(GETPID as u64).0 != u64::from(std::process::id()) {
            return 5;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("an exclusive range at offset 0 was refused".into()),
        4 => Err("OFF was refused".into()),
        5 => Err("a call was dispatched after OFF".into()),
        row @ 10..=17 => Err(format!(
            "prctl row {} was not refused as Linux refuses it",
            row - 10
        )),
        other => Err(format!("the prctl child ended with {other}")),
    }
}

/// Where the image carries this program for the exec of **inherit**.
const CARRIED_AT: &str = "/bin/sud-test";
/// The argument that makes the program the exec'd child of **inherit**.
const CHILD: &str = "--exec-child";

/// What the program does when **inherit** execs it: map the page again and
/// call from it, which must run, the dispatch of the program before the
/// exec gone.
fn exec_child() -> c_int {
    if setup_page_only().is_err() {
        return 2;
    }
    if from_page(GETPID as u64).0 != u64::from(std::process::id()) {
        return 3;
    }
    0
}

/// The page and its stubs alone, for the exec'd child.
fn setup_page_only() -> Step {
    // SAFETY: as in `setup`.
    let page = unsafe {
        libc::mmap(
            PAGE_AT as *mut c_void,
            4096,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if page != PAGE_AT as *mut c_void {
        return Err("no page".into());
    }
    // SAFETY: the page is ours and writable.
    unsafe { std::ptr::copy_nonoverlapping(STUB.as_ptr(), page.cast::<u8>(), STUB.len()) };
    PAGE.store(page as u64, Ordering::Release);
    Ok(())
}

/// **inherit**: a thread and a fork child of a dispatching thread start
/// with it off, and an exec clears it.
fn inherit() -> Step {
    let pid = u64::from(std::process::id());
    select(BLOCK);
    let before = TAKEN.load(Ordering::Acquire);
    let thread = std::thread::spawn(|| from_page(GETPID as u64).0);
    let from_thread = thread
        .join()
        .map_err(|_| "the thread panicked".to_string())?;
    select(ALLOW);
    if TAKEN.load(Ordering::Acquire) != before || from_thread != pid {
        return Err(format!(
            "a new thread started dispatched (its getpid {from_thread:#x}, {} signals)",
            TAKEN.load(Ordering::Acquire) - before
        ));
    }
    select(BLOCK);
    let child = fork_with(|| {
        if from_page(GETPID as u64).0 == u64::from(std::process::id()) {
            0
        } else {
            3
        }
    })?;
    select(ALLOW);
    match reap(child) {
        0 => {}
        other => {
            return Err(format!(
                "a fork child started dispatched: it ended with {other}"
            ));
        }
    }
    // The exec: dispatch for calls from the page only, with no selector, so
    // that nothing before the exec is dispatched and anything from the page
    // after it would be, were it kept.
    let path = std::ffi::CString::new(CARRIED_AT).map_err(|_| "a NUL".to_string())?;
    let argument = std::ffi::CString::new(CHILD).map_err(|_| "a NUL".to_string())?;
    let child = fork_with(|| {
        let page = PAGE.load(Ordering::Acquire);
        if dispatch(DISPATCH_INCLUSIVE_ON, page as c_ulong, 4096, 0).0 != 0 {
            return 4;
        }
        let argv = [path.as_ptr(), argument.as_ptr(), std::ptr::null()];
        // SAFETY: a NUL-terminated path and argument vector, alive for the call.
        let _ = unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
        100 + errno().min(155)
    })?;
    match reap(child) {
        0 => Ok(()),
        4 => Err("an inclusive range of the page was refused".into()),
        BY_SIGSYS => Err("exec kept syscall user dispatch".into()),
        failed @ 100..=255 => Err(format!("the exec failed with errno {}", failed - 100)),
        other => Err(format!("the exec'd child ended with {other}")),
    }
}

/// **seccomp**: a filter that traps `getpid` with data 5 and kills
/// 0xffff: a dispatched getpid is code 2, one from the text code 1, and a
/// dispatched 0xffff is not killed.
fn seccomp_order() -> Step {
    let child = fork_with(|| {
        if install_handler().is_err() || dispatch_on().is_err() {
            return 2;
        }
        // SAFETY: plain integers.
        if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return 2;
        }
        #[repr(C)]
        struct SockFilter {
            code: u16,
            jt: u8,
            jf: u8,
            k: u32,
        }
        #[repr(C)]
        struct SockFprog {
            len: u16,
            filter: *const SockFilter,
        }
        let insn = |code, jt, jf, k| SockFilter { code, jt, jf, k };
        let program = [
            insn(0x20, 0, 0, 0),
            insn(0x15, 0, 1, GETPID as u32),
            insn(0x06, 0, 0, RET_TRAP | 5),
            insn(0x15, 0, 1, UNKNOWN as u32),
            insn(0x06, 0, 0, RET_KILL_PROCESS),
            insn(0x06, 0, 0, RET_ALLOW),
        ];
        let prog = SockFprog {
            len: program.len() as u16,
            filter: program.as_ptr(),
        };
        // SAFETY: `prog` and its program are alive for the call.
        if unsafe { libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, 0, &raw const prog) } != 0 {
            return 3;
        }
        let page = PAGE.load(Ordering::Acquire);
        SELECTOR.store(BLOCK, Ordering::SeqCst);
        let before = TAKEN.load(Ordering::Acquire);
        let _ = from_page(GETPID as u64);
        let dispatched = expect_dispatch(before, GETPID as u64, page + AFTER as u64, OWN_ARCH);
        SELECTOR.store(ALLOW, Ordering::SeqCst);
        if dispatched.is_err() {
            return 4;
        }
        // From the text: seccomp's trap, code 1, data 5.
        // SAFETY: getpid takes nothing.
        let _ = unsafe { libc::syscall(GETPID) };
        if CODE.load(Ordering::Acquire) != SYS_SECCOMP || ERRNO.load(Ordering::Acquire) != 5 {
            return 5;
        }
        // A dispatched 0xffff meets no filter, so the kill does not happen.
        SELECTOR.store(BLOCK, Ordering::SeqCst);
        let _ = from_page(UNKNOWN);
        SELECTOR.store(ALLOW, Ordering::SeqCst);
        if CODE.load(Ordering::Acquire) != SYS_USER_DISPATCH {
            return 6;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("the filter was refused".into()),
        4 => Err("a dispatched getpid under a trapping filter was not a dispatch".into()),
        5 => Err("a getpid from the text did not meet the filter's trap".into()),
        BY_SIGSYS => Err("a dispatched call met the seccomp filter".into()),
        other => Err(format!("the seccomp child ended with {other}")),
    }
}

/// The NT numbers a Windows program's own code was seen making directly
/// under Wine (x64 numbering: NtWaitForSingleObject, NtQueryInformationProcess,
/// NtWaitForMultipleObjects32, NtDelayExecution, NtQueryInformationThread).
#[cfg(target_arch = "x86_64")]
const NT_CALLS: [u64; 5] = [0x04, 0x19, 0x1A, 0x34, 0x25];
/// Where such code sat: a mapped image's address, high in the user half,
/// as the observed calls came from 0x6ffff8... and 0x7fff....
#[cfg(target_arch = "x86_64")]
const IMAGE_AT: [usize; 2] = [0x6FFF_F800_0000, 0x7FF0_0000_0000];

/// **nt**: from pages where a Windows image sits, each NT number the
/// workload calls directly is dispatched with the number in the context,
/// and the program resumes with the handler's result.
fn nt() -> Step {
    #[cfg(target_arch = "x86_64")]
    for at in IMAGE_AT {
        // SAFETY: an anonymous mapping at a fixed address that must be free.
        let page = unsafe {
            libc::mmap(
                at as *mut c_void,
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if page != at as *mut c_void {
            return Err(format!("mmap at {at:#x} gave {page:?}, errno {}", errno()));
        }
        // SAFETY: the page is ours, writable and 4096 bytes long.
        unsafe { std::ptr::copy_nonoverlapping(STUB.as_ptr(), page.cast::<u8>(), STUB.len()) };
        // SAFETY: the page is ours.
        if unsafe { libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            return Err(format!("mprotect at {at:#x} failed, errno {}", errno()));
        }
        // SAFETY: the page holds `STUB`, mapped executable.
        let stub: Stub = unsafe { std::mem::transmute::<usize, Stub>(at) };
        for number in NT_CALLS {
            let mut after = [0_u64; 2];
            select(BLOCK);
            let before = TAKEN.load(Ordering::Acquire);
            // SAFETY: as above; `after` is alive for the call.
            let answer = unsafe { stub(number as usize, &raw mut after) } as u64;
            select(ALLOW);
            expect_dispatch(before, number, at as u64 + AFTER as u64, OWN_ARCH)?;
            if FOUND_RESULT.load(Ordering::Acquire) != number {
                return Err(format!(
                    "NT call {number:#x} from {at:#x} reached the handler with another RAX"
                ));
            }
            if answer != EMULATED {
                return Err(format!(
                    "NT call {number:#x} from {at:#x} resumed with {answer:#x}"
                ));
            }
        }
    }
    Ok(())
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some(CHILD) {
        std::process::exit(exec_child());
    }
    let steps: [Named; 12] = [
        ("setup", setup),
        ("allow", allow),
        ("block", block),
        ("context", context),
        ("special", special),
        ("compat", compat),
        ("blocked", blocked),
        ("kills", kills),
        ("prctl", prctl_rows),
        ("inherit", inherit),
        ("seccomp", seccomp_order),
        ("nt", nt),
    ];
    for (name, step) in steps {
        if let Err(what) = step() {
            say(&format!("sud: FAILED {name}: {what}"));
            std::process::exit(1);
        }
        say(&format!("sud: {name} ok"));
    }
    say("sud: all ok");
}
