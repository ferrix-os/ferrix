//! The x86-64 vDSO's code: `clock_gettime`, `gettimeofday` and `time`.
//!
//! Assembled into the kernel's read-only data, never run where it is: the
//! Linux personality copies the bytes into the image `src/lib/kernel/vdso` lays out,
//! at `CODE_AT`, and a program runs them wherever that page lands. So
//! nothing in them may name an address. Each function finds the data page by
//! its own address -- `lea` of the code's first byte, less `CODE_AT` and a
//! page -- and every jump and call is to a label in the same bytes, which the
//! assembler resolves to a distance and leaves no relocation for. Each
//! function has a slot of its own at a fixed offset, so that the boot check's
//! program can call each without a symbol lookup; the assembler refuses a
//! function that outgrows its slot.
//!
//! # The arithmetic
//!
//! The kernel's clock is `counter * 1e9 / hz` in 128 bits
//! (`src/kernel/src/timer.rs`), and this is the same number: `mul` leaves the
//! 128-bit product in `rdx:rax` and `div` divides it by the frequency. The
//! quotient fits in 64 bits for five hundred years of uptime, past which
//! `div` would fault; the kernel's own answer saturates there instead. The
//! TSC is read after an `lfence`, so that the read cannot be done early, ahead
//! of whatever the program did before asking.
//!
//! The real-time clock adds the offset and, as the kernel does, answers zero
//! rather than a time before the epoch. `CLOCK_REALTIME`, its coarse form and
//! `CLOCK_TAI` are that; `CLOCK_MONOTONIC`, its raw and coarse forms and
//! `CLOCK_BOOTTIME` are the counter alone -- the kernel answers the same for
//! each, and the coarse ones are no cheaper here. Any other clock, and every
//! call when the data page says `MODE_SYSCALL`, is the system call, whose
//! return value -- zero, or a negated `errno` -- is the function's, as Linux's
//! vDSO answers.
//!
//! Each entry starts with `endbr64`, a no-op on a processor without CET and
//! what one with indirect branch tracking on requires of a function called
//! through a pointer, which is how every C library calls these.

use core::arch::global_asm;

use ferrix_vdso::{
    CODE_AT, Function, IMAGE_BYTES, VVAR_COUNTER_HZ, VVAR_MODE, VVAR_REALTIME_OFFSET,
};

/// `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_MONOTONIC_COARSE` and
/// `CLOCK_BOOTTIME`, as bits.
const MONOTONIC_CLOCKS: u32 = (1 << 1) | (1 << 4) | (1 << 6) | (1 << 7);

/// `CLOCK_REALTIME`, `CLOCK_REALTIME_COARSE` and `CLOCK_TAI`, as bits.
const REALTIME_CLOCKS: u32 = (1 << 0) | (1 << 5) | (1 << 11);

/// Where `gettimeofday` starts in the code; `clock_gettime` starts at zero.
const GETTIMEOFDAY_AT: usize = 128;

/// Where `time` starts in the code.
const TIME_AT: usize = 256;

global_asm!(
    ".pushsection .rodata.ferrix_vdso,\"a\"",
    ".balign 16",
    ".balign 16",
    ".globl ferrix_vdso_code",
    "ferrix_vdso_code:",
    ".Lvdso_start:",
    // int clock_gettime(clockid_t clock (edi), struct timespec *ts (rsi))
    ".Lvdso_clock_gettime:",
    "endbr64",
    "lea r8, [rip + .Lvdso_start]",
    "sub r8, {vvar_back}",
    "cmp qword ptr [r8 + {mode}], 0",
    "je .Lvdso_clock_gettime_syscall",
    "cmp edi, 31",
    "ja .Lvdso_clock_gettime_syscall",
    "mov eax, 1",
    "mov ecx, edi",
    "shl eax, cl",
    "test eax, {monotonic}",
    "jnz .Lvdso_clock_gettime_monotonic",
    "test eax, {realtime}",
    "jz .Lvdso_clock_gettime_syscall",
    "call .Lvdso_now",
    "add rax, qword ptr [r8 + {offset}]",
    "jns .Lvdso_clock_gettime_store",
    "xor eax, eax",
    "jmp .Lvdso_clock_gettime_store",
    ".Lvdso_clock_gettime_monotonic:",
    "call .Lvdso_now",
    ".Lvdso_clock_gettime_store:",
    "xor edx, edx",
    "mov ecx, 1000000000",
    "div rcx",
    "mov qword ptr [rsi], rax",
    "mov qword ptr [rsi + 8], rdx",
    "xor eax, eax",
    "ret",
    ".Lvdso_clock_gettime_syscall:",
    "mov eax, 228",
    "syscall",
    "ret",
    // int gettimeofday(struct timeval *tv (rdi), struct timezone *tz (rsi))
    ".org .Lvdso_start + {gettimeofday}, 0xcc",
    ".Lvdso_gettimeofday:",
    "endbr64",
    "lea r8, [rip + .Lvdso_start]",
    "sub r8, {vvar_back}",
    "cmp qword ptr [r8 + {mode}], 0",
    "je .Lvdso_gettimeofday_syscall",
    // A time zone is asked for so rarely that the kernel may as well answer.
    "test rsi, rsi",
    "jnz .Lvdso_gettimeofday_syscall",
    "test rdi, rdi",
    "jz .Lvdso_gettimeofday_done",
    "call .Lvdso_now",
    "add rax, qword ptr [r8 + {offset}]",
    "jns .Lvdso_gettimeofday_split",
    "xor eax, eax",
    ".Lvdso_gettimeofday_split:",
    "xor edx, edx",
    "mov ecx, 1000",
    "div rcx",
    "xor edx, edx",
    "mov ecx, 1000000",
    "div rcx",
    "mov qword ptr [rdi], rax",
    "mov qword ptr [rdi + 8], rdx",
    ".Lvdso_gettimeofday_done:",
    "xor eax, eax",
    "ret",
    ".Lvdso_gettimeofday_syscall:",
    "mov eax, 96",
    "syscall",
    "ret",
    // time_t time(time_t *t (rdi))
    ".org .Lvdso_start + {time}, 0xcc",
    ".Lvdso_time:",
    "endbr64",
    "lea r8, [rip + .Lvdso_start]",
    "sub r8, {vvar_back}",
    "cmp qword ptr [r8 + {mode}], 0",
    "je .Lvdso_time_syscall",
    "call .Lvdso_now",
    "add rax, qword ptr [r8 + {offset}]",
    "jns .Lvdso_time_split",
    "xor eax, eax",
    ".Lvdso_time_split:",
    "xor edx, edx",
    "mov ecx, 1000000000",
    "div rcx",
    "test rdi, rdi",
    "jz .Lvdso_time_done",
    "mov qword ptr [rdi], rax",
    ".Lvdso_time_done:",
    "ret",
    ".Lvdso_time_syscall:",
    "mov eax, 201",
    "syscall",
    "ret",
    // The counter's nanoseconds in rax, from the data page at r8; rcx and rdx
    // are lost.
    ".Lvdso_now:",
    "lfence",
    "rdtsc",
    "shl rdx, 32",
    "or rax, rdx",
    "mov ecx, 1000000000",
    "mul rcx",
    "div qword ptr [r8 + {hz}]",
    "ret",
    ".Lvdso_end:",
    ".balign 8",
    ".globl ferrix_vdso_len",
    "ferrix_vdso_len:",
    ".quad .Lvdso_end - .Lvdso_start",
    ".popsection",
    vvar_back = const CODE_AT + IMAGE_BYTES,
    gettimeofday = const GETTIMEOFDAY_AT,
    time = const TIME_AT,
    mode = const VVAR_MODE,
    hz = const VVAR_COUNTER_HZ,
    offset = const VVAR_REALTIME_OFFSET,
    monotonic = const MONOTONIC_CLOCKS,
    realtime = const REALTIME_CLOCKS,
);

unsafe extern "C" {
    /// The code's first byte.
    static ferrix_vdso_code: u8;
    /// Its length in bytes.
    static ferrix_vdso_len: u64;
}

/// The functions the code exports, where [`vdso_spec`]'s bytes start each.
const FUNCTIONS: [Function<'static>; 3] = [
    Function {
        name: "__vdso_clock_gettime",
        alias: Some("clock_gettime"),
        offset: 0,
    },
    Function {
        name: "__vdso_gettimeofday",
        alias: Some("gettimeofday"),
        offset: GETTIMEOFDAY_AT,
    },
    Function {
        name: "__vdso_time",
        alias: Some("time"),
        offset: TIME_AT,
    },
];

/// The vDSO's machine code and what it exports: always, on x86-64.
pub fn vdso_spec() -> Option<ferrix_vdso::Spec<'static>> {
    // SAFETY: (BOOT-DATA) a quadword the assembly above defines in read-only data, never
    // written, and valid for the kernel's life.
    let len = usize::try_from(unsafe { ferrix_vdso_len }).ok()?;
    let start = &raw const ferrix_vdso_code;
    // SAFETY: (BOOT-DATA) `start` is the code's first byte and `len` its length, both from
    // the same assembly: the bytes lie in read-only data, one section, for the
    // kernel's life, and nothing writes them.
    let code = unsafe { core::slice::from_raw_parts(start, len) };
    Some(ferrix_vdso::Spec {
        machine: ferrix_vdso::EM_X86_64,
        code,
        functions: &FUNCTIONS,
    })
}

/// Whether the vDSO's code can read the kernel's counter itself: when the
/// counter is the TSC, which ring 3 reads with `rdtsc`, and not the HPET.
pub fn vdso_can_read_counter() -> bool {
    super::clock::counter_is_tsc()
}

/// A program that finds the vDSO through `AT_SYSINFO_EHDR` and holds each of
/// its functions to the system call it stands for, made on either side of it:
/// 120 when right.
///
/// The functions are called at their slots: `clock_gettime` at `CODE_AT`
/// from the image's start, `gettimeofday` 128 bytes on and `time` 256, which
/// the check requires of the image before it runs this. Its statuses: 2 no
/// `AT_SYSINFO_EHDR`; for the `n`th clock of `CLOCK_MONOTONIC`,
/// `CLOCK_REALTIME`, `CLOCK_BOOTTIME`, `CLOCK_MONOTONIC_RAW`,
/// `CLOCK_MONOTONIC_COARSE`, `CLOCK_REALTIME_COARSE` and `CLOCK_TAI`, `10n +
/// 11` the vDSO's call failed, `+ 12` it answered before the first system
/// call, `+ 13` after the second, `+ 14` nanoseconds out of range; 81 to 84
/// the same for `gettimeofday`; 91 `time` returned one thing and stored
/// another, 92 and 93 it was out of order; 95 an unknown clock was not
/// `-EINVAL`; 98 a system call failed.
///
/// ```text
///   rsi = the auxiliary vector, past argc, argv and envp
///   find AT_SYSINFO_EHDR (33) -> r12 ; r12 += 0x800
///   for clock in [1, 0, 7, 4, 6, 5, 11]:
///     clock_gettime(clock, &a) as a system call
///     call r12(clock, &b) == 0 ; b.tv_nsec < 1e9
///     clock_gettime(clock, &c) as a system call
///     a <= b <= c, each as nanoseconds
///   gettimeofday(&a, NULL) ; call r12 + 128(&b, NULL) == 0 ; gettimeofday(&c)
///     a <= b <= c, each as microseconds ; b.tv_usec < 1e6
///   time(NULL) -> s ; call r12 + 256(&t) -> v == t ; time(NULL) -> e
///     s <= v <= e
///   call r12(100, &a) == -22 ; exit_group(120)
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub const USER_VDSO_PROGRAM: &[u8] = &[
    0x48, 0x8b, 0x0c, 0x24, 0x48, 0x8d, 0x74, 0xcc, 0x10, 0x48, 0x8b, 0x06, 0x48, 0x83, 0xc6, 0x08,
    0x48, 0x85, 0xc0, 0x75, 0xf4, 0x48, 0x8b, 0x06, 0x48, 0x85, 0xc0, 0x0f, 0x84, 0xff, 0x01, 0x00,
    0x00, 0x48, 0x83, 0xf8, 0x21, 0x74, 0x06, 0x48, 0x83, 0xc6, 0x10, 0xeb, 0xe8, 0x4c, 0x8b, 0x66,
    0x08, 0x4d, 0x85, 0xe4, 0x0f, 0x84, 0xe6, 0x01, 0x00, 0x00, 0x49, 0x81, 0xc4, 0x00, 0x08, 0x00,
    0x00, 0x48, 0x83, 0xec, 0x40, 0x49, 0xbf, 0x01, 0x00, 0x07, 0x04, 0x06, 0x05, 0x0b, 0x00, 0x45,
    0x31, 0xf6, 0x45, 0x0f, 0xb6, 0xef, 0xb8, 0xe4, 0x00, 0x00, 0x00, 0x44, 0x89, 0xef, 0x48, 0x8d,
    0x34, 0x24, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0xc5, 0x01, 0x00, 0x00, 0x44, 0x89, 0xef,
    0x48, 0x8d, 0x74, 0x24, 0x10, 0x41, 0xff, 0xd4, 0xba, 0x01, 0x00, 0x00, 0x00, 0x48, 0x85, 0xc0,
    0x0f, 0x85, 0xa1, 0x01, 0x00, 0x00, 0xb8, 0xe4, 0x00, 0x00, 0x00, 0x44, 0x89, 0xef, 0x48, 0x8d,
    0x74, 0x24, 0x20, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x94, 0x01, 0x00, 0x00, 0xba, 0x04,
    0x00, 0x00, 0x00, 0x48, 0x81, 0x7c, 0x24, 0x18, 0x00, 0xca, 0x9a, 0x3b, 0x0f, 0x83, 0x75, 0x01,
    0x00, 0x00, 0x4c, 0x8b, 0x04, 0x24, 0x4d, 0x69, 0xc0, 0x00, 0xca, 0x9a, 0x3b, 0x4c, 0x03, 0x44,
    0x24, 0x08, 0x4c, 0x8b, 0x4c, 0x24, 0x10, 0x4d, 0x69, 0xc9, 0x00, 0xca, 0x9a, 0x3b, 0x4c, 0x03,
    0x4c, 0x24, 0x18, 0x4c, 0x8b, 0x54, 0x24, 0x20, 0x4d, 0x69, 0xd2, 0x00, 0xca, 0x9a, 0x3b, 0x4c,
    0x03, 0x54, 0x24, 0x28, 0xba, 0x02, 0x00, 0x00, 0x00, 0x4d, 0x39, 0xc8, 0x0f, 0x87, 0x35, 0x01,
    0x00, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x4d, 0x39, 0xd1, 0x0f, 0x87, 0x27, 0x01, 0x00, 0x00,
    0x41, 0xff, 0xc6, 0x49, 0xc1, 0xef, 0x08, 0x41, 0x83, 0xfe, 0x07, 0x0f, 0x82, 0x41, 0xff, 0xff,
    0xff, 0xb8, 0x60, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x3c, 0x24, 0x31, 0xf6, 0x0f, 0x05, 0x48, 0x85,
    0xc0, 0x0f, 0x85, 0x0b, 0x01, 0x00, 0x00, 0x48, 0x8d, 0x7c, 0x24, 0x10, 0x31, 0xf6, 0x49, 0x8d,
    0x84, 0x24, 0x80, 0x00, 0x00, 0x00, 0xff, 0xd0, 0xbf, 0x51, 0x00, 0x00, 0x00, 0x48, 0x85, 0xc0,
    0x0f, 0x85, 0xf1, 0x00, 0x00, 0x00, 0xb8, 0x60, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x7c, 0x24, 0x20,
    0x31, 0xf6, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0xd5, 0x00, 0x00, 0x00, 0xbf, 0x54, 0x00,
    0x00, 0x00, 0x48, 0x81, 0x7c, 0x24, 0x18, 0x40, 0x42, 0x0f, 0x00, 0x0f, 0x83, 0xc6, 0x00, 0x00,
    0x00, 0x4c, 0x8b, 0x04, 0x24, 0x4d, 0x69, 0xc0, 0x40, 0x42, 0x0f, 0x00, 0x4c, 0x03, 0x44, 0x24,
    0x08, 0x4c, 0x8b, 0x4c, 0x24, 0x10, 0x4d, 0x69, 0xc9, 0x40, 0x42, 0x0f, 0x00, 0x4c, 0x03, 0x4c,
    0x24, 0x18, 0x4c, 0x8b, 0x54, 0x24, 0x20, 0x4d, 0x69, 0xd2, 0x40, 0x42, 0x0f, 0x00, 0x4c, 0x03,
    0x54, 0x24, 0x28, 0xbf, 0x52, 0x00, 0x00, 0x00, 0x4d, 0x39, 0xc8, 0x0f, 0x87, 0x86, 0x00, 0x00,
    0x00, 0xbf, 0x53, 0x00, 0x00, 0x00, 0x4d, 0x39, 0xd1, 0x77, 0x7c, 0xb8, 0xc9, 0x00, 0x00, 0x00,
    0x31, 0xff, 0x0f, 0x05, 0x49, 0x89, 0xc5, 0x48, 0x8d, 0x7c, 0x24, 0x10, 0x49, 0x8d, 0x84, 0x24,
    0x00, 0x01, 0x00, 0x00, 0xff, 0xd0, 0x49, 0x89, 0xc6, 0xbf, 0x5b, 0x00, 0x00, 0x00, 0x4c, 0x3b,
    0x74, 0x24, 0x10, 0x75, 0x52, 0xb8, 0xc9, 0x00, 0x00, 0x00, 0x31, 0xff, 0x0f, 0x05, 0xbf, 0x5c,
    0x00, 0x00, 0x00, 0x4d, 0x39, 0xf5, 0x77, 0x3f, 0xbf, 0x5d, 0x00, 0x00, 0x00, 0x49, 0x39, 0xc6,
    0x77, 0x35, 0xbf, 0x64, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x34, 0x24, 0x41, 0xff, 0xd4, 0x48, 0x83,
    0xf8, 0xea, 0xbf, 0x5f, 0x00, 0x00, 0x00, 0x75, 0x1e, 0xbf, 0x78, 0x00, 0x00, 0x00, 0xeb, 0x17,
    0xbf, 0x02, 0x00, 0x00, 0x00, 0xeb, 0x10, 0x41, 0x6b, 0xfe, 0x0a, 0x01, 0xd7, 0x83, 0xc7, 0x0a,
    0xeb, 0x05, 0xbf, 0x62, 0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];
