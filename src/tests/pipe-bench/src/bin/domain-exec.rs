//! The first program of `pipe-bench`'s domain run: made by `process_create`
//! in a job marked as one speculation domain, where it has no Linux start-up
//! stack -- no `argc`, `argv` or auxiliary vector, which a C library's start
//! reads -- so it does one thing, `execve` of `/bin/pipe-bench member`. The
//! process, and so its domain, stays across the `execve`, and the program
//! that runs then has the stack every Linux program starts with.

#![no_std]
#![no_main]

/// `execve` on x86-64.
#[cfg(target_arch = "x86_64")]
const EXECVE: usize = 59;
/// `exit_group` on x86-64.
#[cfg(target_arch = "x86_64")]
const EXIT_GROUP: usize = 231;

/// A system call of three arguments.
#[cfg(target_arch = "x86_64")]
fn syscall3(number: usize, a: usize, b: usize, c: usize) -> usize {
    let ret;
    // SAFETY: a system call; the kernel reads only what the arguments name.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

/// The program, its arguments, and an empty environment.
static PATH: [u8; 16] = *b"/bin/pipe-bench\0";
/// `argv[1]`, which tells the program it is the member.
static MEMBER: [u8; 7] = *b"member\0";

/// The entry: `execve`, and `exit_group(127)` if it comes back.
///
/// # Safety
///
/// Called only as the process's entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() -> ! {
    let argv: [usize; 3] = [PATH.as_ptr() as usize, MEMBER.as_ptr() as usize, 0];
    let envp: [usize; 1] = [0];
    let _ = syscall3(
        EXECVE,
        PATH.as_ptr() as usize,
        argv.as_ptr() as usize,
        envp.as_ptr() as usize,
    );
    let _ = syscall3(EXIT_GROUP, 127, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Nothing here can panic; a panic would end the process as `exit_group`
/// does.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    let _ = syscall3(EXIT_GROUP, 126, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}
