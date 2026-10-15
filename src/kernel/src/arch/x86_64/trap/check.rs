//! A program's own exceptions end it with the signal Linux gives each,
//! `arch_prctl` refuses what it must, and compatibility mode is entered and
//! left only the ways `docs/I386.md` §3.3 allows.
//!
//! Verification, not the handlers: a file of its own so that the manifest
//! counts it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`). Each case is a real program, built into an ELF and
//! run by the Linux personality's loader, because only an instruction a
//! program executes in ring 3 takes the path under test: the stub, `classify`,
//! the generic dispatcher's user fault and `fault_signal`, and the kill that
//! ends the program with the signal. The programs are fixtures, and live here
//! rather than beside the product's in `ferrix_kernel_core::arch::x86_64::trap::super`.
//!
//! Before this, the one program the boot ended by a fault wrote through a null
//! pointer (`object/check.rs`), so a divide error, an invalid opcode, a
//! privileged instruction and a floating-point exception each reached a line
//! of `fault_signal` no boot had run, and nothing had touched a file mapping
//! past its file's end, which the dispatcher turns into `SIGBUS` itself.

#![cfg(target_arch = "x86_64")]

use ferrix_linux_abi::types::{SIGBUS, SIGFPE, SIGILL, SIGSEGV};

use ferrix_kernel_core::arch::x86_64::cpu;
use crate::console::println;

/// `divl %ecx` with `%ecx` zero: `#DE`, vector 0.
///
/// ```text
///   xorl %ecx, %ecx ; movl $1, %eax ; xorl %edx, %edx
///   divl %ecx                                  ; #DE
///   movl $231, %eax ; movl $97, %edi ; syscall ; exit_group(97)
/// ```
///
/// Every program here ends with that `exit_group(97)`, which only a program
/// the exception did not end reaches. Assembled by GNU `as` and read back
/// out of the object file.
const DIVIDE: &[u8] = &[
    0x31, 0xc9, 0xb8, 0x01, 0x00, 0x00, 0x00, 0x31, 0xd2, 0xf7, 0xf1, 0xb8, 0xe7, 0x00, 0x00, 0x00,
    0xbf, 0x61, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// `ud2`: `#UD`, vector 6.
const INVALID: &[u8] = &[
    0x0f, 0x0b, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x61, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// A read of an address no mapping covers: `#PF`, vector 14, with the present
/// bit clear. The null write `object/check.rs` makes is the other case.
///
/// ```text
///   movabsq $0x100000000000, %rax ; movq (%rax), %rax
/// ```
const UNMAPPED: &[u8] = &[
    0x48, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x48, 0x8b, 0x00, 0xb8, 0xe7, 0x00,
    0x00, 0x00, 0xbf, 0x61, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// `hlt` in ring 3: `#GP`, vector 13, which `classify` has no case of its
/// own for and hands on by name.
const PRIVILEGED: &[u8] = &[
    0xf4, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x61, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// `1.0 / 0.0` on the x87 with the zero-divide exception unmasked, and an
/// `fwait` to deliver it: `#MF`, vector 16, where `CR0.NE` asks for native
/// reporting, as every firmware since the 486 leaves it.
///
/// ```text
///   fninit ; subq $8, %rsp ; fnstcw (%rsp) ; andw $0xfffb, (%rsp) ; fldcw (%rsp)
///   fldz ; fld1 ; fdivrp %st, %st(1)           ; 1.0 / 0.0, pending
///   fwait                                      ; #MF
/// ```
///
/// The x87 rather than SSE's `#XM`, which is the more common case: QEMU's
/// TCG does not raise SIMD floating-point exceptions at all, and the program
/// ran on to its `exit_group(97)`.
const X87: &[u8] = &[
    0xdb, 0xe3, 0x48, 0x83, 0xec, 0x08, 0xd9, 0x3c, 0x24, 0x66, 0x83, 0x24, 0x24, 0xfb, 0xd9, 0x2c,
    0x24, 0xd9, 0xee, 0xd9, 0xe8, 0xde, 0xf1, 0x9b, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x61, 0x00,
    0x00, 0x00, 0x0f, 0x05,
];

/// A read of the second page of a shared mapping of a one-page file: `#PF`,
/// which the address space answers with the file's end rather than a missing
/// mapping, so the dispatcher sends `SIGBUS` and not what `fault_signal` would
/// say. Exits with 96 if the file or the mapping could not be made.
///
/// ```text
///   leaq name(%rip), %rdi ; xorl %esi, %esi ; movl $319, %eax ; syscall
///   testq %rax, %rax ; js 1f ; movq %rax, %r8           ; memfd_create
///   movq %rax, %rdi ; movl $4096, %esi ; movl $77, %eax ; syscall
///   testq %rax, %rax ; jnz 1f                           ; ftruncate to a page
///   xorl %edi, %edi ; movl $8192, %esi ; movl $1, %edx ; movl $1, %r10d
///   xorl %r9d, %r9d ; movl $9, %eax ; syscall            ; two pages, shared
///   cmpq $-4096, %rax ; ja 1f
///   movq 4096(%rax), %rax                                ; past the file
///   movl $231, %eax ; movl $97, %edi ; syscall
/// 1: movl $231, %eax ; movl $96, %edi ; syscall
/// name: .asciz "bus"
/// ```
const PAST_END: &[u8] = &[
    0x48, 0x8d, 0x3d, 0x68, 0x00, 0x00, 0x00, 0x31, 0xf6, 0xb8, 0x3f, 0x01, 0x00, 0x00, 0x0f, 0x05,
    0x48, 0x85, 0xc0, 0x78, 0x4e, 0x49, 0x89, 0xc0, 0x48, 0x89, 0xc7, 0xbe, 0x00, 0x10, 0x00, 0x00,
    0xb8, 0x4d, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x37, 0x31, 0xff, 0xbe, 0x00,
    0x20, 0x00, 0x00, 0xba, 0x01, 0x00, 0x00, 0x00, 0x41, 0xba, 0x01, 0x00, 0x00, 0x00, 0x45, 0x31,
    0xc9, 0xb8, 0x09, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0xf0, 0xff, 0xff, 0x77, 0x13,
    0x48, 0x8b, 0x80, 0x00, 0x10, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x61, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x60, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x62,
    0x75, 0x73, 0x00,
];

/// A `SIGSEGV` handler's view of the fault that raised it. The program
/// installs an `SA_SIGINFO` handler and writes to `0x100000000008`, which
/// nothing maps. The handler exits 0 when the `ucontext`'s `sigcontext` has
/// `trapno` 14, `err` 6 (a write, from ring 3, of a page not present) and
/// `cr2` the address, and the `siginfo`'s `si_addr` is the address too;
/// 1 to 4 name the first that is not, 9 a failed `rt_sigaction`, and 8 a
/// write that did not fault. Wine's handler reads all three: it tells a
/// page fault from every other trap by `trapno`, and a write from a read by
/// `err` (`docs/WINE.md` §5).
///
/// ```text
///   subq $32, %rsp
///   leaq handler(%rip), %rax ; movq %rax, (%rsp)
///   movq $0x04000004, 8(%rsp)
///   leaq restorer(%rip), %rax ; movq %rax, 16(%rsp)
///   movq $0, 24(%rsp)
///   movl $13, %eax ; movl $11, %edi ; movq %rsp, %rsi ; xorl %edx, %edx ; movl $8, %r10d
///   syscall
///   movl $9, %edi ; testq %rax, %rax ; jnz exit
///   movabsq $0x100000000008, %rbx
///   movq %rax, (%rbx)
///   movl $8, %edi ; jmp exit
/// handler:
///   movl $1, %edi ; cmpq $14, 200(%rdx) ; jne exit
///   movl $2, %edi ; cmpq $6, 192(%rdx) ; jne exit
///   movl $3, %edi ; movabsq $0x100000000008, %rax ; cmpq %rax, 216(%rdx) ; jne exit
///   movl $4, %edi ; cmpq %rax, 16(%rsi) ; jne exit
///   xorl %edi, %edi
/// exit:
///   movl $231, %eax ; syscall
/// restorer:
///   movl $15, %eax ; syscall
/// ```
const SEGV_CONTEXT: &[u8] = &[
    0x48, 0x83, 0xec, 0x20, 0x48, 0x8d, 0x05, 0x57, 0x00, 0x00, 0x00, 0x48, 0x89, 0x04, 0x24, 0x48,
    0xc7, 0x44, 0x24, 0x08, 0x04, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05, 0x8d, 0x00, 0x00, 0x00, 0x48,
    0x89, 0x44, 0x24, 0x10, 0x48, 0xc7, 0x44, 0x24, 0x18, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x0d, 0x00,
    0x00, 0x00, 0xbf, 0x0b, 0x00, 0x00, 0x00, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0x41, 0xba, 0x08, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0xbf, 0x09, 0x00, 0x00, 0x00, 0x48, 0x85, 0xc0, 0x75, 0x57, 0x48, 0xbb,
    0x08, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x48, 0x89, 0x03, 0xbf, 0x08, 0x00, 0x00, 0x00,
    0xeb, 0x43, 0xbf, 0x01, 0x00, 0x00, 0x00, 0x48, 0x83, 0xba, 0xc8, 0x00, 0x00, 0x00, 0x0e, 0x75,
    0x34, 0xbf, 0x02, 0x00, 0x00, 0x00, 0x48, 0x83, 0xba, 0xc0, 0x00, 0x00, 0x00, 0x06, 0x75, 0x25,
    0xbf, 0x03, 0x00, 0x00, 0x00, 0x48, 0xb8, 0x08, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x48,
    0x39, 0x82, 0xd8, 0x00, 0x00, 0x00, 0x75, 0x0d, 0xbf, 0x04, 0x00, 0x00, 0x00, 0x48, 0x39, 0x46,
    0x10, 0x75, 0x02, 0x31, 0xff, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x0f, 0x00, 0x00,
    0x00, 0x0f, 0x05,
];

/// What `arch_prctl` refuses. `ARCH_SET_FS` and `ARCH_SET_GS` with
/// `0xffff800000000000` have to be `EPERM`, and `ARCH_SET_GS` with the
/// non-canonical `0x800000000000` too; request `0x1000`, which is none,
/// `EINVAL`. The program exits with 0, or with 1 for `ARCH_SET_FS`'s
/// refusal, 2 for `ARCH_SET_GS`'s, 3 for the unknown request.
///
/// ```text
///   movl $158, %eax ; movl $0x1002, %edi ; movabsq $0xffff800000000000, %rsi
///   syscall ; cmpq $-1, %rax ; jne 1f
///   movl $158, %eax ; movl $0x1001, %edi ; movabsq $0xffff800000000000, %rsi
///   syscall ; cmpq $-1, %rax ; jne 2f
///   movl $158, %eax ; movl $0x1001, %edi ; movabsq $0x0000800000000000, %rsi
///   syscall ; cmpq $-1, %rax ; jne 2f
///   movl $158, %eax ; movl $0x1000, %edi ; xorl %esi, %esi
///   syscall ; cmpq $-22, %rax ; jne 3f
///   xorl %edi, %edi ; jmp 9f
/// 1: movl $1, %edi ; jmp 9f
/// 2: movl $2, %edi ; jmp 9f
/// 3: movl $3, %edi
/// 9: movl $231, %eax ; syscall
/// ```
const PRCTL_REFUSALS: &[u8] = &[
    0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x02, 0x10, 0x00, 0x00, 0x48, 0xbe, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x80, 0xff, 0xff, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0xff, 0x75, 0x50, 0xb8, 0x9e, 0x00, 0x00,
    0x00, 0xbf, 0x01, 0x10, 0x00, 0x00, 0x48, 0xbe, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xff, 0xff,
    0x0f, 0x05, 0x48, 0x83, 0xf8, 0xff, 0x75, 0x3b, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x10,
    0x00, 0x00, 0x48, 0xbe, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x83,
    0xf8, 0xff, 0x75, 0x1f, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x00, 0x10, 0x00, 0x00, 0x31, 0xf6,
    0x0f, 0x05, 0x48, 0x83, 0xf8, 0xea, 0x75, 0x12, 0x31, 0xff, 0xeb, 0x13, 0xbf, 0x01, 0x00, 0x00,
    0x00, 0xeb, 0x0c, 0xbf, 0x02, 0x00, 0x00, 0x00, 0xeb, 0x05, 0xbf, 0x03, 0x00, 0x00, 0x00, 0xb8,
    0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// `arch_prctl`'s two readers. `ARCH_GET_FS` and `ARCH_GET_GS` into address
/// 0 have to be `EFAULT`. Then a word on the stack is made the `GS` base and
/// must read back through `%gs:0` and through `ARCH_GET_GS`, the next word
/// the `FS` base and back through `ARCH_GET_FS`, and the `GS` base must
/// still be the first. The program exits with 0, or with the number of the
/// step that failed: 4 the unwritable address, 5 setting `GS`, 6 reading it
/// back, 7 `FS`.
///
/// ```text
///   movl $158, %eax ; movl $0x1003, %edi ; xorl %esi, %esi
///   syscall ; cmpq $-14, %rax ; jne 4f
///   movl $158, %eax ; movl $0x1004, %edi ; xorl %esi, %esi
///   syscall ; cmpq $-14, %rax ; jne 4f
///   leaq -64(%rsp), %rbx ; movabsq $0x1122334455667788, %rax ; movq %rax, (%rbx)
///   movl $158, %eax ; movl $0x1001, %edi ; movq %rbx, %rsi
///   syscall ; testq %rax, %rax ; jne 5f
///   movq %gs:0, %rax ; cmpq (%rbx), %rax ; jne 5f
///   movl $158, %eax ; movl $0x1004, %edi ; leaq 8(%rbx), %rsi
///   syscall ; testq %rax, %rax ; jne 6f
///   cmpq %rbx, 8(%rbx) ; jne 6f
///   movl $158, %eax ; movl $0x1002, %edi ; leaq 16(%rbx), %rsi
///   syscall ; testq %rax, %rax ; jne 7f
///   movl $158, %eax ; movl $0x1003, %edi ; leaq 24(%rbx), %rsi
///   syscall ; testq %rax, %rax ; jne 7f
///   leaq 16(%rbx), %rax ; cmpq %rax, 24(%rbx) ; jne 7f
///   movl $158, %eax ; movl $0x1004, %edi ; leaq 8(%rbx), %rsi
///   syscall ; cmpq %rbx, 8(%rbx) ; jne 6f
///   xorl %edi, %edi ; jmp 9f
/// 4: movl $4, %edi ; jmp 9f
/// 5: movl $5, %edi ; jmp 9f
/// 6: movl $6, %edi ; jmp 9f
/// 7: movl $7, %edi
/// 9: movl $231, %eax ; syscall
/// ```
const PRCTL_BASES: &[u8] = &[
    0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x03, 0x10, 0x00, 0x00, 0x31, 0xf6, 0x0f, 0x05, 0x48, 0x83,
    0xf8, 0xf2, 0x0f, 0x85, 0xb5, 0x00, 0x00, 0x00, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x04, 0x10,
    0x00, 0x00, 0x31, 0xf6, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0xf2, 0x0f, 0x85, 0x9d, 0x00, 0x00, 0x00,
    0x48, 0x8d, 0x5c, 0x24, 0xc0, 0x48, 0xb8, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x48,
    0x89, 0x03, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x10, 0x00, 0x00, 0x48, 0x89, 0xde, 0x0f,
    0x05, 0x48, 0x85, 0xc0, 0x75, 0x7e, 0x65, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00, 0x48,
    0x3b, 0x03, 0x75, 0x70, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x04, 0x10, 0x00, 0x00, 0x48, 0x8d,
    0x73, 0x08, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x62, 0x48, 0x39, 0x5b, 0x08, 0x75, 0x5c, 0xb8,
    0x9e, 0x00, 0x00, 0x00, 0xbf, 0x02, 0x10, 0x00, 0x00, 0x48, 0x8d, 0x73, 0x10, 0x0f, 0x05, 0x48,
    0x85, 0xc0, 0x75, 0x4e, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x03, 0x10, 0x00, 0x00, 0x48, 0x8d,
    0x73, 0x18, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x39, 0x48, 0x8d, 0x43, 0x10, 0x48, 0x39, 0x43,
    0x18, 0x75, 0x2f, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x04, 0x10, 0x00, 0x00, 0x48, 0x8d, 0x73,
    0x08, 0x0f, 0x05, 0x48, 0x39, 0x5b, 0x08, 0x75, 0x12, 0x31, 0xff, 0xeb, 0x1a, 0xbf, 0x04, 0x00,
    0x00, 0x00, 0xeb, 0x13, 0xbf, 0x05, 0x00, 0x00, 0x00, 0xeb, 0x0c, 0xbf, 0x06, 0x00, 0x00, 0x00,
    0xeb, 0x05, 0xbf, 0x07, 0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// An i386 program, in an `ELFCLASS32` `EM_386` image: it writes a line
/// through `int $0x80`, and exits with 42 only if the write returned its
/// length, a write to descriptor -1 returned `-EBADF` whole in `EAX`, and
/// number 90 -- i386's `old_mmap`, whose arguments are a block in memory
/// that nothing reads, so it is not carried -- answered `-ENOSYS`
/// (`docs/I386.md` §3.2). 43 otherwise. 90 must stay unmapped for as long as
/// this check asks it: whoever maps it moves the probe to a number that is
/// still unmapped, as I3 moved it here from 39, `mkdir`.
///
/// ```text
///   movl $4, %eax ; movl $1, %ebx ; call 1f
/// 1: popl %ecx ; addl $(msg - 1b), %ecx ; movl $16, %edx ; int $0x80
///   cmpl $16, %eax ; jne 9f                     ; write(1, msg, 16)
///   movl $4, %eax ; movl $-1, %ebx ; int $0x80
///   cmpl $-9, %eax ; jne 9f                     ; write(-1): EBADF
///   movl $90, %eax ; int $0x80
///   cmpl $-38, %eax ; jne 9f                    ; unmapped: ENOSYS
///   movl $252, %eax ; movl $42, %ebx ; int $0x80 ; exit_group(42)
/// 9: movl $252, %eax ; movl $43, %ebx ; int $0x80
/// msg: .ascii "hello from i386\n"
/// ```
///
/// Assembled by GNU `as` in `.code32`, linked at the image's entry, and read
/// back with `objdump -m i386`.
const HELLO_I386: &[u8] = &[
    0xb8, 0x04, 0x00, 0x00, 0x00, 0xbb, 0x01, 0x00, 0x00, 0x00, 0xe8, 0x00, 0x00, 0x00, 0x00, 0x59,
    0x81, 0xc1, 0x4a, 0x00, 0x00, 0x00, 0xba, 0x10, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x3d, 0x10, 0x00,
    0x00, 0x00, 0x75, 0x29, 0xb8, 0x04, 0x00, 0x00, 0x00, 0xbb, 0xff, 0xff, 0xff, 0xff, 0xcd, 0x80,
    0x83, 0xf8, 0xf7, 0x75, 0x18, 0xb8, 0x5a, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x83, 0xf8, 0xda, 0x75,
    0x0c, 0xb8, 0xfc, 0x00, 0x00, 0x00, 0xbb, 0x2a, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xb8, 0xfc, 0x00,
    0x00, 0x00, 0xbb, 0x2b, 0x00, 0x00, 0x00, 0xcd, 0x80, b'h', b'e', b'l', b'l', b'o', b' ', b'f',
    b'r', b'o', b'm', b' ', b'i', b'3', b'8', b'6', b'\n',
];

/// i386's `socketcall` (`docs/I386.md` I4), which glibc on i386 sends every
/// socket operation through: a sub-call number and a block of 32-bit
/// arguments. The program makes a pair of connected Unix sockets through it,
/// sends a byte down one with `send` and reads it from the other with `recv`,
/// then asks for sub-call 0, which must be `-EINVAL`, sub-call 1 with no
/// block, `-EFAULT`, and `recvmmsg` (19), which has no handler here and must
/// be `-ENOSYS`. Exits 42, or 60 to 66 for the step that failed.
///
/// ```text
///   subl $48, %esp                      ; block at 0, sv at 32, bytes at 40 and 44
///   movl $1, 0(%esp) ; movl $1, 4(%esp) ; movl $0, 8(%esp)
///   leal 32(%esp), %eax ; movl %eax, 12(%esp)
///   movl $102, %eax ; movl $8, %ebx ; movl %esp, %ecx ; int $0x80
///   movl $60, %edi ; testl %eax, %eax ; jne 9f        ; SOCKETPAIR == 0
///   movb $0x5a, 40(%esp) ; movl 32(%esp), %eax ; movl %eax, 0(%esp)
///   leal 40(%esp), %eax ; movl %eax, 4(%esp) ; movl $1, 8(%esp) ; movl $0, 12(%esp)
///   movl $102, %eax ; movl $9, %ebx ; movl %esp, %ecx ; int $0x80
///   movl $61, %edi ; cmpl $1, %eax ; jne 9f          ; SEND == 1
///   movb $0, 44(%esp) ; movl 36(%esp), %eax ; movl %eax, 0(%esp)
///   leal 44(%esp), %eax ; movl %eax, 4(%esp)
///   movl $102, %eax ; movl $10, %ebx ; movl %esp, %ecx ; int $0x80
///   movl $62, %edi ; cmpl $1, %eax ; jne 9f          ; RECV == 1
///   movl $63, %edi ; cmpb $0x5a, 44(%esp) ; jne 9f   ; the byte sent
///   movl $102, %eax ; xorl %ebx, %ebx ; movl %esp, %ecx ; int $0x80
///   movl $64, %edi ; cmpl $-22, %eax ; jne 9f        ; sub-call 0: EINVAL
///   movl $102, %eax ; movl $1, %ebx ; xorl %ecx, %ecx ; int $0x80
///   movl $65, %edi ; cmpl $-14, %eax ; jne 9f        ; no block: EFAULT
///   movl $102, %eax ; movl $19, %ebx ; movl %esp, %ecx ; int $0x80
///   movl $66, %edi ; cmpl $-38, %eax ; jne 9f        ; RECVMMSG: ENOSYS
///   movl $42, %edi
/// 9: movl %edi, %ebx ; movl $252, %eax ; int $0x80   ; exit_group(%edi)
/// ```
///
/// Assembled by GNU `as --32` in `.code32`, and read back with `objdump -m
/// i386`.
const SOCKETCALL_I386: &[u8] = &[
    0x83, 0xec, 0x30, 0xc7, 0x04, 0x24, 0x01, 0x00, 0x00, 0x00, 0xc7, 0x44, 0x24, 0x04, 0x01, 0x00,
    0x00, 0x00, 0xc7, 0x44, 0x24, 0x08, 0x00, 0x00, 0x00, 0x00, 0x8d, 0x44, 0x24, 0x20, 0x89, 0x44,
    0x24, 0x0c, 0xb8, 0x66, 0x00, 0x00, 0x00, 0xbb, 0x08, 0x00, 0x00, 0x00, 0x89, 0xe1, 0xcd, 0x80,
    0xbf, 0x3c, 0x00, 0x00, 0x00, 0x85, 0xc0, 0x0f, 0x85, 0xc2, 0x00, 0x00, 0x00, 0xc6, 0x44, 0x24,
    0x28, 0x5a, 0x8b, 0x44, 0x24, 0x20, 0x89, 0x04, 0x24, 0x8d, 0x44, 0x24, 0x28, 0x89, 0x44, 0x24,
    0x04, 0xc7, 0x44, 0x24, 0x08, 0x01, 0x00, 0x00, 0x00, 0xc7, 0x44, 0x24, 0x0c, 0x00, 0x00, 0x00,
    0x00, 0xb8, 0x66, 0x00, 0x00, 0x00, 0xbb, 0x09, 0x00, 0x00, 0x00, 0x89, 0xe1, 0xcd, 0x80, 0xbf,
    0x3d, 0x00, 0x00, 0x00, 0x83, 0xf8, 0x01, 0x0f, 0x85, 0x82, 0x00, 0x00, 0x00, 0xc6, 0x44, 0x24,
    0x2c, 0x00, 0x8b, 0x44, 0x24, 0x24, 0x89, 0x04, 0x24, 0x8d, 0x44, 0x24, 0x2c, 0x89, 0x44, 0x24,
    0x04, 0xb8, 0x66, 0x00, 0x00, 0x00, 0xbb, 0x0a, 0x00, 0x00, 0x00, 0x89, 0xe1, 0xcd, 0x80, 0xbf,
    0x3e, 0x00, 0x00, 0x00, 0x83, 0xf8, 0x01, 0x75, 0x56, 0xbf, 0x3f, 0x00, 0x00, 0x00, 0x80, 0x7c,
    0x24, 0x2c, 0x5a, 0x75, 0x4a, 0xb8, 0x66, 0x00, 0x00, 0x00, 0x31, 0xdb, 0x89, 0xe1, 0xcd, 0x80,
    0xbf, 0x40, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xea, 0x75, 0x35, 0xb8, 0x66, 0x00, 0x00, 0x00, 0xbb,
    0x01, 0x00, 0x00, 0x00, 0x31, 0xc9, 0xcd, 0x80, 0xbf, 0x41, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xf2,
    0x75, 0x1d, 0xb8, 0x66, 0x00, 0x00, 0x00, 0xbb, 0x13, 0x00, 0x00, 0x00, 0x89, 0xe1, 0xcd, 0x80,
    0xbf, 0x42, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xda, 0x75, 0x05, 0xbf, 0x2a, 0x00, 0x00, 0x00, 0x89,
    0xfb, 0xb8, 0xfc, 0x00, 0x00, 0x00, 0xcd, 0x80,
];

/// What [`HELLO_I386`] exits with when every call answered as it had to.
const HELLO_I386_STATUS: i32 = 42;

/// ELF's `e_machine` for i386.
const EM_386: u16 = 3;

/// A 64-bit program's `int $0x80` is an i386 call, as on Linux: 20 is
/// `getpid` there (and `writev` in x86-64's table), and must answer what a
/// `SYSCALL` `getpid` does. Then `dup` of `0x1_0000_0001`, whose upper half a
/// 32-bit program could never have set, must duplicate descriptor 1, and the
/// copy close. Exits 0, or 1 if `getpid` disagreed, or 2 if the upper half
/// was read.
///
/// ```text
///   movl $39, %eax ; syscall ; movq %rax, %r12  ; getpid, x86-64's number
///   movl $20, %eax ; int $0x80                  ; getpid, i386's number
///   cmpq %rax, %r12 ; jne 1f
///   movabsq $0x100000001, %rbx ; movl $41, %eax ; int $0x80   ; dup(1)
///   testq %rax, %rax ; js 2f
///   movl %eax, %ebx ; movl $6, %eax ; int $0x80 ; testq %rax, %rax ; jnz 2f
///   movl $231, %eax ; xorl %edi, %edi ; syscall
/// 1: movl $231, %eax ; movl $1, %edi ; syscall
/// 2: movl $231, %eax ; movl $2, %edi ; syscall
/// ```
const INT80_FROM_64: &[u8] = &[
    0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x49, 0x89, 0xc4, 0xb8, 0x14, 0x00, 0x00, 0x00, 0xcd,
    0x80, 0x49, 0x39, 0xc4, 0x75, 0x2d, 0x48, 0xbb, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0xb8, 0x29, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x48, 0x85, 0xc0, 0x78, 0x23, 0x89, 0xc3, 0xb8, 0x06,
    0x00, 0x00, 0x00, 0xcd, 0x80, 0x48, 0x85, 0xc0, 0x75, 0x15, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x31,
    0xff, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8,
    0xe7, 0x00, 0x00, 0x00, 0xbf, 0x02, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// A 64-bit program far-returns into the 32-bit code segment and issues
/// `SYSCALL` there, which goes to `IA32_CSTAR` on AMD -- and under QEMU's
/// TCG -- and is `#UD` on Intel. Either way it must not enter the kernel
/// anywhere but the stub that answers `-ENOSYS` (`docs/I386.md` §3.3). Back
/// in 64-bit mode by a far jump, it exits 0 for `-ENOSYS` and 1 for anything
/// else; `SIGILL` ends it on Intel.
///
/// ```text
/// .code64: pushq $0x23 ; pushq $compat ; lretq
/// .code32: compat: syscall ; movl %eax, %esi ; ljmp $0x33, $back
/// .code64: back: xorl %edi, %edi ; cmpl $-38, %esi ; je 1f ; movl $1, %edi
///          1: movl $231, %eax ; syscall
/// ```
///
/// Assembled by GNU `as`, linked at the image's entry, and read back in each
/// half's own mode.
const COMPAT_SYSCALL: &[u8] = &[
    0x6a, 0x23, 0x68, 0x09, 0x01, 0x40, 0x00, 0x48, 0xcb, 0x0f, 0x05, 0x89, 0xc6, 0xea, 0x14, 0x01,
    0x40, 0x00, 0x33, 0x00, 0x31, 0xff, 0x83, 0xfe, 0xda, 0x74, 0x05, 0xbf, 0x01, 0x00, 0x00, 0x00,
    0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// The same far return, then `SYSENTER`: `#GP` with `IA32_SYSENTER_CS` zero
/// on Intel and under TCG, `#UD` on AMD, which has no `SYSENTER` in long mode.
/// Either ends the program with a signal; reaching `exit_group(97)` means it
/// entered the kernel somewhere and came back.
///
/// ```text
/// .code64: pushq $0x23 ; pushq $compat ; lretq
/// .code32: compat: sysenter ; ljmp $0x33, $back
/// .code64: back: movl $231, %eax ; movl $97, %edi ; syscall
/// ```
const COMPAT_SYSENTER: &[u8] = &[
    0x6a, 0x23, 0x68, 0x09, 0x01, 0x40, 0x00, 0x48, 0xcb, 0x0f, 0x34, 0xea, 0x12, 0x01, 0x40, 0x00,
    0x33, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x61, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// An i386 program's thread pointer, as musl sets one up: `set_thread_area`
/// with entry -1, base the image's data page, limit `0xfffff`, flags `0x51`;
/// the entry chosen, 12, loaded into `%gs` as selector `0x63`. It then reads
/// `%gs:0` two hundred times with a `sched_yield` between each, and must
/// see its own base's word every time -- [`TLS_I386_HIGH`] runs the same
/// program with a base four bytes on, pinned to the same processor, so a
/// descriptor or selector that did not travel with its thread shows as the
/// other's word. Then `get_thread_area(12)` must give the base back, a
/// second entry -1 must be 13, and code, conforming code, an absent segment
/// and a 16-bit one must each be `EINVAL` (certification review, T.ESCALATE
/// path 7). `lm`, which a 32-bit caller's garbage may set, must be ignored:
/// the third entry, 14, takes it, and `lar` on its selector from ring 3 must
/// find DPL 3 and no L bit. Exits 42, or for the step that failed: 40 the
/// call, 41 the entry chosen, 43 the read back, 44 the second entry, 45, 47,
/// 48 and 49 the refusals, 46 the word through `%gs`, 39 the `lm` entry and
/// 38 its access rights.
///
/// ```text
///   subl $16, %esp ; user_desc { -1, BASE, 0xfffff, 0x51 } at (%esp)
///   movl $243, %eax ; movl %esp, %ebx ; int $0x80        ; set_thread_area
///   movl $40, %ebx ; testl %eax, %eax ; jnz 9f
///   movl (%esp), %edx ; movl $41, %ebx ; cmpl $12, %edx ; jne 9f
///   leal 3(,%edx,8), %edx ; movw %dx, %gs ; movl $200, %esi
/// 1: movl %gs:0, %eax ; movl $46, %ebx ; cmpl $WANT, %eax ; jne 9f
///   movl $158, %eax ; int $0x80 ; decl %esi ; jnz 1b      ; sched_yield
///   entry 12 at (%esp) ; movl $244, %eax ; movl %esp, %ebx ; int $0x80
///   movl $43, %ebx ; testl %eax, %eax ; jnz 9f ; cmpl $BASE, 4(%esp) ; jne 9f
///   user_desc { -1, BASE, 0xfffff, 0x51 } ; set_thread_area
///   movl $44, %ebx ; cmpl $13, (%esp) ; jne 9f
///   entry -1, flags 0x55 (code) ; set_thread_area
///   movl $45, %ebx ; cmpl $-22, %eax ; jne 9f
///   the same for 0x57 (conforming code, 47), 0x71 (absent, 48), 0x50
///   (16-bit, 49)
///   entry -1, flags 0xd1 (lm) ; set_thread_area ; movl $39, %ebx
///   testl %eax, %eax ; jnz 9f ; cmpl $14, (%esp) ; jne 9f
///   movl $0x73, %ecx ; larl %ecx, %eax ; movl $38, %ebx ; jnz 9f
///   testl $0x200000, %eax ; jnz 9f                        ; no L bit
///   shrl $13, %eax ; andl $3, %eax ; cmpl $3, %eax ; jne 9f  ; DPL 3
///   movl $42, %ebx
/// 9: movl $252, %eax ; int $0x80                          ; exit_group(%ebx)
/// ```
///
/// This one's `BASE` is the data page, `0x410000`, whose first word is
/// `"stag"` of `DATA_MARK`. Assembled by GNU `as --32` twice with the two
/// `--defsym` pairs, linked at the image's entry, read back with
/// `objdump -m i386`; the two differ in their bases and words.
const TLS_I386_LOW: &[u8] = &[
    0x83, 0xec, 0x10, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x04, 0x00, 0x00,
    0x41, 0x00, 0xc7, 0x44, 0x24, 0x08, 0xff, 0xff, 0x0f, 0x00, 0xc7, 0x44, 0x24, 0x0c, 0x51, 0x00,
    0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x28, 0x00, 0x00, 0x00,
    0x85, 0xc0, 0x0f, 0x85, 0x86, 0x01, 0x00, 0x00, 0x8b, 0x14, 0x24, 0xbb, 0x29, 0x00, 0x00, 0x00,
    0x83, 0xfa, 0x0c, 0x0f, 0x85, 0x75, 0x01, 0x00, 0x00, 0x8d, 0x14, 0xd5, 0x03, 0x00, 0x00, 0x00,
    0x8e, 0xea, 0xbe, 0xc8, 0x00, 0x00, 0x00, 0x65, 0xa1, 0x00, 0x00, 0x00, 0x00, 0xbb, 0x2e, 0x00,
    0x00, 0x00, 0x3d, 0x73, 0x74, 0x61, 0x67, 0x0f, 0x85, 0x51, 0x01, 0x00, 0x00, 0xb8, 0x9e, 0x00,
    0x00, 0x00, 0xcd, 0x80, 0x4e, 0x75, 0xe0, 0xc7, 0x04, 0x24, 0x0c, 0x00, 0x00, 0x00, 0xc7, 0x44,
    0x24, 0x04, 0x00, 0x00, 0x00, 0x00, 0xb8, 0xf4, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb,
    0x2b, 0x00, 0x00, 0x00, 0x85, 0xc0, 0x0f, 0x85, 0x22, 0x01, 0x00, 0x00, 0x81, 0x7c, 0x24, 0x04,
    0x00, 0x00, 0x41, 0x00, 0x0f, 0x85, 0x14, 0x01, 0x00, 0x00, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff,
    0xff, 0xc7, 0x44, 0x24, 0x04, 0x00, 0x00, 0x41, 0x00, 0xc7, 0x44, 0x24, 0x08, 0xff, 0xff, 0x0f,
    0x00, 0xc7, 0x44, 0x24, 0x0c, 0x51, 0x00, 0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3,
    0xcd, 0x80, 0xbb, 0x2c, 0x00, 0x00, 0x00, 0x83, 0x3c, 0x24, 0x0d, 0x0f, 0x85, 0xdd, 0x00, 0x00,
    0x00, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0x55, 0x00, 0x00, 0x00,
    0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x2d, 0x00, 0x00, 0x00, 0x83, 0xf8,
    0xea, 0x0f, 0x85, 0xb7, 0x00, 0x00, 0x00, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44,
    0x24, 0x0c, 0x57, 0x00, 0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb,
    0x2f, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xea, 0x0f, 0x85, 0x91, 0x00, 0x00, 0x00, 0xc7, 0x04, 0x24,
    0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0x71, 0x00, 0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00,
    0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x30, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xea, 0x75, 0x6f, 0xc7,
    0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0x50, 0x00, 0x00, 0x00, 0xb8, 0xf3,
    0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x31, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xea, 0x75,
    0x4d, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0xd1, 0x00, 0x00, 0x00,
    0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x27, 0x00, 0x00, 0x00, 0x85, 0xc0,
    0x75, 0x2c, 0x83, 0x3c, 0x24, 0x0e, 0x75, 0x26, 0xb9, 0x73, 0x00, 0x00, 0x00, 0x0f, 0x02, 0xc1,
    0xbb, 0x26, 0x00, 0x00, 0x00, 0x75, 0x17, 0xa9, 0x00, 0x00, 0x20, 0x00, 0x75, 0x10, 0xc1, 0xe8,
    0x0d, 0x83, 0xe0, 0x03, 0x83, 0xf8, 0x03, 0x75, 0x05, 0xbb, 0x2a, 0x00, 0x00, 0x00, 0xb8, 0xfc,
    0x00, 0x00, 0x00, 0xcd, 0x80,
];

/// [`TLS_I386_LOW`] with `BASE` `0x410004`, whose word is `"e7-l"`.
const TLS_I386_HIGH: &[u8] = &[
    0x83, 0xec, 0x10, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x04, 0x04, 0x00,
    0x41, 0x00, 0xc7, 0x44, 0x24, 0x08, 0xff, 0xff, 0x0f, 0x00, 0xc7, 0x44, 0x24, 0x0c, 0x51, 0x00,
    0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x28, 0x00, 0x00, 0x00,
    0x85, 0xc0, 0x0f, 0x85, 0x86, 0x01, 0x00, 0x00, 0x8b, 0x14, 0x24, 0xbb, 0x29, 0x00, 0x00, 0x00,
    0x83, 0xfa, 0x0c, 0x0f, 0x85, 0x75, 0x01, 0x00, 0x00, 0x8d, 0x14, 0xd5, 0x03, 0x00, 0x00, 0x00,
    0x8e, 0xea, 0xbe, 0xc8, 0x00, 0x00, 0x00, 0x65, 0xa1, 0x00, 0x00, 0x00, 0x00, 0xbb, 0x2e, 0x00,
    0x00, 0x00, 0x3d, 0x65, 0x37, 0x2d, 0x6c, 0x0f, 0x85, 0x51, 0x01, 0x00, 0x00, 0xb8, 0x9e, 0x00,
    0x00, 0x00, 0xcd, 0x80, 0x4e, 0x75, 0xe0, 0xc7, 0x04, 0x24, 0x0c, 0x00, 0x00, 0x00, 0xc7, 0x44,
    0x24, 0x04, 0x00, 0x00, 0x00, 0x00, 0xb8, 0xf4, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb,
    0x2b, 0x00, 0x00, 0x00, 0x85, 0xc0, 0x0f, 0x85, 0x22, 0x01, 0x00, 0x00, 0x81, 0x7c, 0x24, 0x04,
    0x04, 0x00, 0x41, 0x00, 0x0f, 0x85, 0x14, 0x01, 0x00, 0x00, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff,
    0xff, 0xc7, 0x44, 0x24, 0x04, 0x04, 0x00, 0x41, 0x00, 0xc7, 0x44, 0x24, 0x08, 0xff, 0xff, 0x0f,
    0x00, 0xc7, 0x44, 0x24, 0x0c, 0x51, 0x00, 0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3,
    0xcd, 0x80, 0xbb, 0x2c, 0x00, 0x00, 0x00, 0x83, 0x3c, 0x24, 0x0d, 0x0f, 0x85, 0xdd, 0x00, 0x00,
    0x00, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0x55, 0x00, 0x00, 0x00,
    0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x2d, 0x00, 0x00, 0x00, 0x83, 0xf8,
    0xea, 0x0f, 0x85, 0xb7, 0x00, 0x00, 0x00, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44,
    0x24, 0x0c, 0x57, 0x00, 0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb,
    0x2f, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xea, 0x0f, 0x85, 0x91, 0x00, 0x00, 0x00, 0xc7, 0x04, 0x24,
    0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0x71, 0x00, 0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00,
    0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x30, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xea, 0x75, 0x6f, 0xc7,
    0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0x50, 0x00, 0x00, 0x00, 0xb8, 0xf3,
    0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x31, 0x00, 0x00, 0x00, 0x83, 0xf8, 0xea, 0x75,
    0x4d, 0xc7, 0x04, 0x24, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x44, 0x24, 0x0c, 0xd1, 0x00, 0x00, 0x00,
    0xb8, 0xf3, 0x00, 0x00, 0x00, 0x89, 0xe3, 0xcd, 0x80, 0xbb, 0x27, 0x00, 0x00, 0x00, 0x85, 0xc0,
    0x75, 0x2c, 0x83, 0x3c, 0x24, 0x0e, 0x75, 0x26, 0xb9, 0x73, 0x00, 0x00, 0x00, 0x0f, 0x02, 0xc1,
    0xbb, 0x26, 0x00, 0x00, 0x00, 0x75, 0x17, 0xa9, 0x00, 0x00, 0x20, 0x00, 0x75, 0x10, 0xc1, 0xe8,
    0x0d, 0x83, 0xe0, 0x03, 0x83, 0xf8, 0x03, 0x75, 0x05, 0xbb, 0x2a, 0x00, 0x00, 0x00, 0xb8, 0xfc,
    0x00, 0x00, 0x00, 0xcd, 0x80,
];

/// An i386 program's signals and `fork` (`docs/I386.md` I2b), each checked by
/// what the program sees. It installs an `SA_SIGINFO` handler for `SIGUSR1`
/// and a plain one for `SIGUSR2`, each with its own restorer as musl gives
/// one, and sends itself both. The first handler checks `sig` and the
/// `siginfo`'s `si_signo` and `si_pid`, and writes 0x77 into the saved `EAX`
/// in its `ucontext`; the second writes 0x88 into its `sigcontext`'s. After
/// `rt_sigreturn` and `sigreturn` each `kill` must have "returned" that
/// value, and `ESI` must have come through. Then it forks: the child exits
/// with 23, and `wait4` must reap it with that status. Exits 42, or 50 to 59
/// for the step that failed.
///
/// ```text
///   rt_sigaction(10, {rt_handler, SA_SIGINFO|SA_RESTORER, rt_restorer}, 0, 8)
///   rt_sigaction(12, {plain_handler, SA_RESTORER, plain_restorer}, 0, 8)
///   %ebp = getpid() ; %esi = 0x5eed ; kill(%ebp, 10) == 0x77 ; %esi == 0x5eed
///   kill(%ebp, 12) == 0x88
///   fork() == 0 ? exit_group(23) : wait4(pid, &status, 0, 0) == pid,
///   (status >> 8) & 0xff == 23 ; exit_group(42)
/// rt_handler: 4(%esp) == 10 ; pinfo->si_signo == 10 ; pinfo[12] == %ebp
///   puc->uc_mcontext.ax (puc + 64) = 0x77 ; ret
/// rt_restorer: movl $173, %eax ; int $0x80
/// plain_handler: 4(%esp) == 12 ; sc.ax (52(%esp)) = 0x88 ; ret
/// plain_restorer: popl %eax ; movl $119, %eax ; int $0x80
/// ```
///
/// Assembled by GNU `as --32`, linked at the image's entry, read back with
/// `objdump -m i386`.
const SIGNALS_I386: &[u8] = &[
    0x83, 0xec, 0x20, 0xc7, 0x04, 0x24, 0x2e, 0x02, 0x40, 0x00, 0xc7, 0x44, 0x24, 0x04, 0x04, 0x00,
    0x00, 0x04, 0xc7, 0x44, 0x24, 0x08, 0x5b, 0x02, 0x40, 0x00, 0xc7, 0x44, 0x24, 0x0c, 0x00, 0x00,
    0x00, 0x00, 0xc7, 0x44, 0x24, 0x10, 0x00, 0x00, 0x00, 0x00, 0xb8, 0xae, 0x00, 0x00, 0x00, 0xbb,
    0x0a, 0x00, 0x00, 0x00, 0x89, 0xe1, 0x31, 0xd2, 0xbe, 0x08, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xbf,
    0x32, 0x00, 0x00, 0x00, 0x85, 0xc0, 0x0f, 0x85, 0xd9, 0x00, 0x00, 0x00, 0xc7, 0x04, 0x24, 0x62,
    0x02, 0x40, 0x00, 0xc7, 0x44, 0x24, 0x04, 0x00, 0x00, 0x00, 0x04, 0xc7, 0x44, 0x24, 0x08, 0x7e,
    0x02, 0x40, 0x00, 0xb8, 0xae, 0x00, 0x00, 0x00, 0xbb, 0x0c, 0x00, 0x00, 0x00, 0x89, 0xe1, 0x31,
    0xd2, 0xbe, 0x08, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xbf, 0x33, 0x00, 0x00, 0x00, 0x85, 0xc0, 0x0f,
    0x85, 0xa0, 0x00, 0x00, 0x00, 0xb8, 0x14, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x89, 0xc5, 0x89, 0xc3,
    0xb9, 0x0a, 0x00, 0x00, 0x00, 0xbe, 0xed, 0x5e, 0x00, 0x00, 0xb8, 0x25, 0x00, 0x00, 0x00, 0xcd,
    0x80, 0xbf, 0x34, 0x00, 0x00, 0x00, 0x83, 0xf8, 0x77, 0x75, 0x7a, 0xbf, 0x35, 0x00, 0x00, 0x00,
    0x81, 0xfe, 0xed, 0x5e, 0x00, 0x00, 0x75, 0x6d, 0x89, 0xeb, 0xb9, 0x0c, 0x00, 0x00, 0x00, 0xb8,
    0x25, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xbf, 0x36, 0x00, 0x00, 0x00, 0x3d, 0x88, 0x00, 0x00, 0x00,
    0x75, 0x53, 0xb8, 0x02, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x85, 0xc0, 0x75, 0x0c, 0xb8, 0xfc, 0x00,
    0x00, 0x00, 0xbb, 0x17, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xbf, 0x37, 0x00, 0x00, 0x00, 0x78, 0x35,
    0x89, 0xc3, 0x8d, 0x4c, 0x24, 0x14, 0x31, 0xd2, 0x31, 0xf6, 0xb8, 0x72, 0x00, 0x00, 0x00, 0xcd,
    0x80, 0xbf, 0x38, 0x00, 0x00, 0x00, 0x39, 0xd8, 0x75, 0x1b, 0x8b, 0x44, 0x24, 0x14, 0xc1, 0xe8,
    0x08, 0x25, 0xff, 0x00, 0x00, 0x00, 0xbf, 0x39, 0x00, 0x00, 0x00, 0x83, 0xf8, 0x17, 0x75, 0x05,
    0xbf, 0x2a, 0x00, 0x00, 0x00, 0x89, 0xfb, 0xb8, 0xfc, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x83, 0x7c,
    0x24, 0x04, 0x0a, 0x75, 0x1a, 0x8b, 0x44, 0x24, 0x08, 0x83, 0x38, 0x0a, 0x75, 0x11, 0x39, 0x68,
    0x0c, 0x75, 0x0c, 0x8b, 0x44, 0x24, 0x0c, 0xc7, 0x40, 0x40, 0x77, 0x00, 0x00, 0x00, 0xc3, 0xb8,
    0xfc, 0x00, 0x00, 0x00, 0xbb, 0x3a, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xb8, 0xad, 0x00, 0x00, 0x00,
    0xcd, 0x80, 0x83, 0x7c, 0x24, 0x04, 0x0c, 0x75, 0x09, 0xc7, 0x44, 0x24, 0x34, 0x88, 0x00, 0x00,
    0x00, 0xc3, 0xb8, 0xfc, 0x00, 0x00, 0x00, 0xbb, 0x3b, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x58, 0xb8,
    0x77, 0x00, 0x00, 0x00, 0xcd, 0x80,
];

/// An i386 program whose `SA_SIGINFO` handler forges one field of its own
/// saved context before returning through `rt_sigreturn` (certification
/// review, T.ESCALATE path 8). The handler's `movl $VALUE, FIELD(%eax)`, with
/// `%eax` the `ucontext`, is patched per case at [`FORGE_FIELD_AT`] and
/// [`FORGE_VALUE_AT`]; a forged frame must end the program with `SIGSEGV`,
/// and a frame changed only in `ax` must return and let it exit 42.
///
/// ```text
///   rt_sigaction(10, {handler, SA_SIGINFO|SA_RESTORER, restorer}, 0, 8)
///   kill(getpid(), 10) ; exit_group(42)
/// handler: movl 12(%esp), %eax ; movl $0x11223344, 0x7f(%eax) ; ret
/// restorer: movl $173, %eax ; int $0x80
/// ```
///
/// Assembled by GNU `as --32` with those placeholders, linked at the image's
/// entry, and read back with `objdump -m i386`.
const FORGE_I386: &[u8] = &[
    0x83, 0xec, 0x20, 0xc7, 0x04, 0x24, 0x60, 0x01, 0x40, 0x00, 0xc7, 0x44, 0x24, 0x04, 0x04, 0x00,
    0x00, 0x04, 0xc7, 0x44, 0x24, 0x08, 0x6c, 0x01, 0x40, 0x00, 0xc7, 0x44, 0x24, 0x0c, 0x00, 0x00,
    0x00, 0x00, 0xc7, 0x44, 0x24, 0x10, 0x00, 0x00, 0x00, 0x00, 0xb8, 0xae, 0x00, 0x00, 0x00, 0xbb,
    0x0a, 0x00, 0x00, 0x00, 0x89, 0xe1, 0x31, 0xd2, 0xbe, 0x08, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xb8,
    0x14, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x89, 0xc3, 0xb9, 0x0a, 0x00, 0x00, 0x00, 0xb8, 0x25, 0x00,
    0x00, 0x00, 0xcd, 0x80, 0xbb, 0x2a, 0x00, 0x00, 0x00, 0xb8, 0xfc, 0x00, 0x00, 0x00, 0xcd, 0x80,
    0x8b, 0x44, 0x24, 0x0c, 0xc7, 0x40, 0x7f, 0x44, 0x33, 0x22, 0x11, 0xc3, 0xb8, 0xad, 0x00, 0x00,
    0x00, 0xcd, 0x80,
];

/// Where [`FORGE_I386`]'s handler keeps the field's offset in the `ucontext`,
/// the `0x7f` placeholder.
const FORGE_FIELD_AT: usize = 102;

/// Where it keeps the value it writes there, the `0x11223344` placeholder.
const FORGE_VALUE_AT: usize = 103;

/// `CR0.NE`: x87 exceptions are reported as `#MF`, not through the legacy
/// `FERR#` line to an interrupt controller.
const CR0_NE: u64 = 1 << 5;

/// The status of a program a signal ended, as `wait4` reports it to a shell.
const fn killed_by(signal: u32) -> i32 {
    128 + signal as i32
}

/// Run every case, and say what each ended with.
///
/// # Errors
///
/// The first program that could not be run, or ended other than it had to.
/// Verifies: `L.x86_64.77`, L.trap.1, `L.x86_64.78`, `L.x86_64.103`, H.TRAP.4
pub(crate) fn run() -> Result<(), &'static str> {
    // What the trap path does after an `execve`: equal only to the same
    // entry, stack and mode. The entry and stack are every architecture's
    // (`crate::trap::check`); the mode is this one's alone, and an entry
    // compared without it would start a 32-bit program in 64-bit mode.
    crate::trap::check::outcomes()?;
    let entered = crate::trap::Outcome::Enter {
        entry: 0x40_1000,
        stack: 0x7fff_f000,
        abi: crate::trap::Abi::Native,
    };
    let other_mode = crate::trap::Outcome::Enter {
        entry: 0x40_1000,
        stack: 0x7fff_f000,
        abi: crate::trap::Abi::Compat,
    };
    // Through `black_box`, or the comparison is folded where it is written
    // and the trap path's own equality never runs.
    if core::hint::black_box(entered) == core::hint::black_box(other_mode) {
        return Err("two entries into a program compared by something but their mode");
    }

    if cpu::read_cr0() & CR0_NE == 0 {
        return Err("CR0.NE is clear, so an x87 exception is not an exception");
    }
    let cases: [(&[u8], &[u8], i32, &'static str); 6] = [
        (
            b"/divide",
            DIVIDE,
            killed_by(SIGFPE),
            "a program's divide error did not end it with SIGFPE",
        ),
        (
            b"/invalid",
            INVALID,
            killed_by(SIGILL),
            "a program's invalid opcode did not end it with SIGILL",
        ),
        (
            b"/unmapped",
            UNMAPPED,
            killed_by(SIGSEGV),
            "a program's read of an unmapped address did not end it with SIGSEGV",
        ),
        (
            b"/privileged",
            PRIVILEGED,
            killed_by(SIGSEGV),
            "a program's privileged instruction did not end it with SIGSEGV",
        ),
        (
            b"/x87",
            X87,
            killed_by(SIGFPE),
            "a program's x87 floating-point exception did not end it with SIGFPE",
        ),
        (
            b"/past-end",
            PAST_END,
            killed_by(SIGBUS),
            "a program's read past the end of a mapped file did not end it with SIGBUS",
        ),
    ];
    for (name, code, expected, problem) in cases {
        let status = run_program(name, code)?;
        if status != expected {
            println!(
                "  fault    {} ended with {status}, not {expected}",
                core::str::from_utf8(name).unwrap_or("?")
            );
            return Err(problem);
        }
    }
    println!(
        "  fault    a program's divide error, invalid opcode, unmapped read, privileged \
         instruction, x87 exception and read past a mapped file's end ended it with SIGFPE, \
         SIGILL, SIGSEGV, SIGSEGV, SIGFPE and SIGBUS"
    );

    check_bases_and_fault_context()?;
    check_compat()?;
    check_forged_64_bit_frames()?;
    check_wow64()
}

/// `arch_prctl`'s refusals and readers, and a `SIGSEGV` handler's view of
/// its fault: three checks, each with its own line.
fn check_bases_and_fault_context() -> Result<(), &'static str> {
    check_fault_context()?;
    check_arch_prctl_refusals()?;
    check_arch_prctl_reads()
}

/// A handler for a fault's signal reads the fault from its `sigcontext`.
///
/// Verifies: `L.x86_64.174`
fn check_fault_context() -> Result<(), &'static str> {
    match run_program(b"/segv-context", SEGV_CONTEXT)? {
        0 => {}
        1 => return Err("a SIGSEGV handler's sigcontext did not say trap 14 for a page fault"),
        2 => return Err("a SIGSEGV handler's sigcontext did not carry the fault's error code"),
        3 => return Err("a SIGSEGV handler's sigcontext did not carry the fault's address in cr2"),
        4 => return Err("a SIGSEGV handler's siginfo did not carry the fault's address"),
        _ => return Err("the SIGSEGV context program did not reach its handler"),
    }
    println!(
        "  sigsegv  a handler for a write to an unmapped page read trap 14, error code 6 and \
         the address in its sigcontext, and the address in its siginfo"
    );
    Ok(())
}

/// `arch_prctl` refuses a base that is not a user address, and a request it
/// does not know.
///
/// Verifies: `L.x86_64.60`
fn check_arch_prctl_refusals() -> Result<(), &'static str> {
    match run_program(b"/prctl", PRCTL_REFUSALS)? {
        0 => {}
        1 => return Err("arch_prctl accepted a kernel address as the thread pointer"),
        2 => return Err("arch_prctl accepted a kernel or non-canonical address as the GS base"),
        _ => return Err("arch_prctl answered an unknown request with something but EINVAL"),
    }
    println!(
        "  prctl    arch_prctl refused a kernel thread pointer and a kernel and a non-canonical GS \
         base with EPERM, and an unknown request with EINVAL"
    );
    Ok(())
}

/// `ARCH_GET_FS` and `ARCH_GET_GS` write the calling thread's bases.
///
/// Verifies: `L.x86_64.173`
fn check_arch_prctl_reads() -> Result<(), &'static str> {
    match run_program(b"/prctl-bases", PRCTL_BASES)? {
        0 => {}
        4 => return Err("arch_prctl's ARCH_GET_FS or ARCH_GET_GS into address 0 was not EFAULT"),
        5 => return Err("a GS base set with ARCH_SET_GS did not read through %gs"),
        6 => return Err("ARCH_GET_GS did not answer the GS base ARCH_SET_GS set"),
        _ => return Err("ARCH_GET_FS did not answer the FS base ARCH_SET_FS set"),
    }
    println!(
        "  prctl    ARCH_GET_FS and ARCH_GET_GS refused an unwritable address with EFAULT; a GS \
         base set read through %gs and ARCH_GET_GS, and an FS base through ARCH_GET_FS"
    );
    Ok(())
}

/// The ways into and out of compatibility mode (`docs/I386.md` I1): an i386
/// program runs and calls through `int $0x80`, a 64-bit program's `int $0x80`
/// is an i386 call, and neither `SYSCALL` nor `SYSENTER` from 32-bit code
/// enters the kernel anywhere else.
/// Verifies: `L.x86_64.2`, `L.x86_64.54`, `L.x86_64.72`, H.TRAP.8
fn check_compat() -> Result<(), &'static str> {
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf32,
        EM_386,
        crate::syscall::image::Shape::Good,
        HELLO_I386,
    );
    let status =
        crate::syscall::exec::run(&file, &[b"/i386"], &[], [0x7e; ferrix_ustack::RANDOM_BYTES])
            .map_err(|_| "an i386 program could not be started")?;
    if status != HELLO_I386_STATUS {
        println!("  i386     the i386 program ended with {status}, not {HELLO_I386_STATUS}");
        return Err(
            "an i386 program's calls through int $0x80 did not answer as the i386 ABI does",
        );
    }

    match run_program(b"/int80", INT80_FROM_64)? {
        0 => {}
        1 => return Err("a 64-bit program's int $0x80 was not an i386 call"),
        _ => return Err("int $0x80 read the upper half of a 64-bit program's register"),
    }

    let tls = check_thread_areas()?;
    check_signals_and_fork()?;
    check_socketcall()?;
    let forged = check_forged_frames()?;

    let syscall_ended = run_program(b"/cstar", COMPAT_SYSCALL)?;
    let syscall_how = match syscall_ended {
        0 => "-ENOSYS",
        status if status == killed_by(SIGILL) => "SIGILL",
        _ => return Err("SYSCALL from compatibility mode was neither refused nor #UD"),
    };
    let sysenter_ended = run_program(b"/sysenter", COMPAT_SYSENTER)?;
    let sysenter_how = match sysenter_ended {
        status if status == killed_by(SIGSEGV) => "SIGSEGV",
        status if status == killed_by(SIGILL) => "SIGILL",
        _ => return Err("SYSENTER from compatibility mode did not end the program"),
    };
    println!(
        "  i386     a 32-bit program wrote and exited through int $0x80, and a 64-bit one's int \
         $0x80 was an i386 call; SYSCALL from 32-bit code answered {syscall_how}, SYSENTER \
         {sysenter_how}"
    );
    println!(
        "  i386     rt_sigreturn ended {forged} programs whose frames forged a kernel or 64-bit \
         code selector, a kernel stack selector, IOPL or virtual-8086 flags, or a data segment \
         naming a kernel slot, the LDT or the TSS, with SIGSEGV, and took an honest change"
    );
    println!(
        "  i386     2 programs on one processor each read their own thread-local segment \
         through %gs {tls} times across sched_yield, set_thread_area chose entries 12 to 14, \
         refused code, conforming, absent and 16-bit segments and took lm without the L bit, \
         and get_thread_area read the base back"
    );
    Ok(())
}

/// [`FORGE_I386`], once for each forgery an i386 `rt_sigreturn` must refuse
/// and once for a frame it must take. Answers how many were refused.
/// Verifies: `L.x86_64.48`
fn check_forged_frames() -> Result<u32, &'static str> {
    use ferrix_linux_abi::sigframe32::sigcontext;
    // `uc_mcontext` is 20 bytes into the `ucontext`, and each field is
    // `sigcontext_32`'s offset past that.
    let mcontext = |field: usize| (20 + field) as u8;
    let forgeries: [(u8, u32, &'static str); 8] = [
        (
            mcontext(sigcontext::CS),
            0x10,
            "a code selector naming the kernel's code",
        ),
        (
            mcontext(sigcontext::CS),
            0x33,
            "a code selector naming 64-bit user code",
        ),
        (
            mcontext(sigcontext::SS),
            0x18,
            "a stack selector naming the kernel's data",
        ),
        (mcontext(sigcontext::FLAGS), 0x3202, "IOPL 3 in the flags"),
        (
            mcontext(sigcontext::FLAGS),
            0x2_0202,
            "virtual-8086 mode in the flags",
        ),
        (
            mcontext(sigcontext::GS),
            0x10,
            "GS naming the kernel's code",
        ),
        (mcontext(sigcontext::FS), 0x07, "FS naming the LDT"),
        (mcontext(sigcontext::DS), 0x43, "DS naming the TSS"),
    ];
    let run = |field: u8, value: u32| -> Result<i32, &'static str> {
        let mut code = FORGE_I386.to_vec();
        let field_at = code
            .get_mut(FORGE_FIELD_AT)
            .ok_or("the forged program is too short")?;
        if *field_at != 0x7f {
            return Err("the forged program's field placeholder is not where it was assembled");
        }
        *field_at = field;
        code.get_mut(FORGE_VALUE_AT..FORGE_VALUE_AT + 4)
            .ok_or("the forged program is too short")?
            .copy_from_slice(&value.to_le_bytes());
        let file = crate::syscall::image::build_with(
            ferrix_elf::Class::Elf32,
            EM_386,
            crate::syscall::image::Shape::Good,
            &code,
        );
        crate::syscall::exec::run(
            &file,
            &[b"/forged"],
            &[],
            [0x7e; ferrix_ustack::RANDOM_BYTES],
        )
        .map_err(|_| "the forged-frame program could not be started")
    };
    // The control first: a frame changed only where a handler may change it
    // returns, or the refusals below would prove nothing.
    if run(mcontext(sigcontext::AX), 0x99)? != HELLO_I386_STATUS {
        return Err("an i386 frame changed only in its saved EAX did not return");
    }
    let mut refused = 0;
    for (field, value, what) in forgeries {
        let status = run(field, value)?;
        if status != killed_by(SIGSEGV) {
            println!("  i386     a frame with {what} ended the program with {status}");
            return Err("rt_sigreturn took a forged i386 frame");
        }
        refused += 1;
    }
    Ok(refused)
}

/// [`SOCKETCALL_I386`]: glibc's one socket entry, unpacked into the call it
/// names, with Linux's refusals in Linux's order.
fn check_socketcall() -> Result<(), &'static str> {
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf32,
        EM_386,
        crate::syscall::image::Shape::Good,
        SOCKETCALL_I386,
    );
    let status = crate::syscall::exec::run(
        &file,
        &[b"/socketcall"],
        &[],
        [0x7e; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the i386 socketcall program could not be started")?;
    let failed = match status {
        HELLO_I386_STATUS => {
            println!(
                "  i386     socketcall made a Unix socket pair, and a byte sent down it with send \
                 came back through recv; sub-call 0 was EINVAL, a missing block EFAULT, and \
                 recvmmsg ENOSYS"
            );
            return Ok(());
        }
        60 => "socketcall(SOCKETPAIR) did not make a pair of Unix sockets",
        61 => "socketcall(SEND) did not send one byte",
        62 => "socketcall(RECV) did not receive one byte",
        63 => "the byte socketcall(RECV) received was not the one sent",
        64 => "socketcall with sub-call 0 was not EINVAL",
        65 => "socketcall with no block to read was not EFAULT",
        66 => "socketcall(RECVMMSG) was not ENOSYS",
        _ => "the i386 socketcall program ended some other way",
    };
    println!("  i386     the socketcall program ended with {status}");
    Err(failed)
}

/// [`SIGNALS_I386`]: both i386 frames and their returns, and a fork.
/// Verifies: `L.x86_64.15`, `L.x86_64.47`, `L.x86_64.64`, `L.x86_64.73`
fn check_signals_and_fork() -> Result<(), &'static str> {
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf32,
        EM_386,
        crate::syscall::image::Shape::Good,
        SIGNALS_I386,
    );
    let status = crate::syscall::exec::run(
        &file,
        &[b"/signals"],
        &[],
        [0x7e; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the i386 signal program could not be started")?;
    let failed = match status {
        HELLO_I386_STATUS => {
            println!(
                "  i386     a 32-bit program's SA_SIGINFO and plain handlers each changed a saved \
                 register through their frames and returned through rt_sigreturn and sigreturn, \
                 and its fork child exited 23 to wait4"
            );
            return Ok(());
        }
        50 | 51 => "rt_sigaction refused a 32-bit program's struct sigaction",
        52 => "an rt_sigframe's saved EAX did not come back through rt_sigreturn",
        53 => "a register the handler did not touch did not survive rt_sigreturn",
        54 => "a sigframe's saved EAX did not come back through sigreturn",
        55 => "fork from a 32-bit program failed",
        56 | 57 => "wait4 did not reap a 32-bit program's fork child with its status",
        58 => "an SA_SIGINFO handler was entered with the wrong signal or siginfo",
        59 => "a plain handler was entered with the wrong signal",
        _ => "the i386 signal program ended some other way",
    };
    println!("  i386     the signal program ended with {status}");
    Err(failed)
}

/// [`TLS_I386_LOW`] and [`TLS_I386_HIGH`] at once, pinned to this processor
/// so that each `sched_yield` can hand it straight to the other. Answers the
/// reads through `%gs` each made.
/// Verifies: `L.x86_64.7`, `L.x86_64.8`, H.SCHED.7
fn check_thread_areas() -> Result<u32, &'static str> {
    /// Reads through `%gs` each program makes.
    const READS: u32 = 200;
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(30_000_000_000);
    let mut started = alloc::vec::Vec::new();
    let programs: [(&[u8], &[u8]); 2] =
        [(b"/tls-low", TLS_I386_LOW), (b"/tls-high", TLS_I386_HIGH)];
    for (name, code) in programs {
        let file = crate::syscall::image::build_with(
            ferrix_elf::Class::Elf32,
            EM_386,
            crate::syscall::image::Shape::Good,
            code,
        );
        let process =
            crate::syscall::exec::load(&file, &[name], &[], [0x7e; ferrix_ustack::RANDOM_BYTES])
                .map_err(|_| "an i386 thread-area program could not be loaded")?;
        let task = crate::syscall::process::start_on(&process, Some(cpu))
            .map_err(|_| "an i386 thread-area program could not be started")?;
        started.push((process, task));
    }
    for (process, _task) in &started {
        match process.wait_for_exit(deadline) {
            Some(HELLO_I386_STATUS) => {}
            Some(46) => {
                return Err("an i386 program read another's thread-local segment through %gs");
            }
            Some(status) => {
                println!("  i386     a thread-area program ended with {status}, not 42");
                return Err("set_thread_area or get_thread_area did not answer as Linux does");
            }
            None => return Err("an i386 thread-area program never ended"),
        }
    }
    Ok(READS)
}

/// Build `code` into a program called `name`, run it, and answer its status.
fn run_program(name: &[u8], code: &[u8]) -> Result<i32, &'static str> {
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf64,
        ferrix_kernel_core::arch::x86_64::ARCH.elf_machine(),
        crate::syscall::image::Shape::Good,
        code,
    );
    crate::syscall::exec::run(&file, &[name], &[], [0x7e; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a program that faults on purpose could not be started")
}

/// 32-bit code inside a 64-bit program, the way Wine runs a 32-bit Windows
/// program in its 64-bit process (`docs/WINE.md` §7). The program installs a
/// 64-bit `SA_SIGINFO` handler for `SIGILL`; takes a thread-local descriptor
/// with an i386 `set_thread_area` through `int $0x80`, the shape Wine's
/// `alloc_fs_sel` asks for (base the program's block, limit `~0`, flags
/// `0x41`); loads its selector into `FS` and sets its thread pointer with
/// `ARCH_SET_FS`, after which `FS` must read 0 and `%fs:0` the new base;
/// then far-returns to the 32-bit code selector on a stack below 4 GiB. There
/// it reads its block through the descriptor, and its `ud2` raises `SIGILL`.
/// The handler must run in 64-bit mode on a 64-bit frame whose `sigcontext`
/// says code selector `0x23` and trap 6; it steps the saved instruction
/// pointer past the `ud2`, and `rt_sigreturn` must resume the 32-bit code in
/// compatibility mode, which far-returns to 64-bit code and exits 0. Its
/// test of `CS` after the `ud2` reads the same in either mode, so a resume in
/// 64-bit code reaches `bad64` and exits by its status, not by a fault.
///
/// The exits: 9 a failed `rt_sigaction`, 10 a failed `set_thread_area`, 11 a
/// failed `ARCH_SET_FS`, 12 `FS` not null after it, 13 `%fs:0` not the new
/// base; 20 the handler never ran, 21 it ran in 32-bit mode, 22 its
/// `sigcontext` did not say `0x23`, 23 not trap 6; 24 the 32-bit code did
/// not read its block through the descriptor, 25 it was not resumed in
/// compatibility mode. Its data lives in the image's writable page.
///
/// ```text
///   subq $32, %rsp
///   leaq handler(%rip), %rax ; movq %rax, (%rsp)
///   movq $0x04000004, 8(%rsp)
///   leaq restorer(%rip), %rax ; movq %rax, 16(%rsp)
///   movq $0, 24(%rsp)
///   movl $13, %eax ; movl $4, %edi ; movq %rsp, %rsi ; xorl %edx, %edx
///   movl $8, %r10d ; syscall
///   movl $9, %edi ; testq %rax, %rax ; jnz exit
///   movl $0x410f00, %ebx
///   movl $-1, (%rbx) ; movl $0x410000, 4(%rbx) ; movl $-1, 8(%rbx)
///   movl $0x41, 12(%rbx)
///   movl $243, %eax ; int $0x80
///   movl $10, %edi ; testl %eax, %eax ; jnz exit
///   movl (%rbx), %eax ; shll $3, %eax ; orl $3, %eax ; movl %eax, 0x410f10
///   movl $0x57303634, 0x410000
///   movl %eax, %fs
///   movl $0x13579bdf, 0x410100
///   movl $158, %eax ; movl $0x1002, %edi ; movl $0x410100, %esi ; syscall
///   movl $11, %edi ; testq %rax, %rax ; jnz exit
///   movl %fs, %eax ; movl $12, %edi ; testw %ax, %ax ; jnz exit
///   movl %fs:0, %eax ; movl $13, %edi ; cmpl $0x13579bdf, %eax ; jne exit
///   movl $0x2b, %eax ; movl %eax, %ds ; movl %eax, %es
///   movq %rsp, 0x410f80 ; movl $0x410e00, %esp
///   pushq $0x23 ; leaq code32(%rip), %rax ; pushq %rax ; lretq
/// back64:
///   movq 0x410f80, %rsp ; movl 0x410f20, %eax
///   movl $20, %edi ; testl %eax, %eax ; jz exit
///   movl $21, %edi ; cmpl $2, %eax ; je exit
///   movl $22, %edi ; cmpl $3, %eax ; je exit
///   movl $23, %edi ; cmpl $4, %eax ; je exit
///   movl $24, %edi ; cmpl $0, 0x410f24 ; jne exit
///   movl $25, %edi ; cmpl $0, 0x410f28 ; jne exit
///   xorl %edi, %edi
/// exit:
///   movl $231, %eax ; syscall
/// handler:
///   movl $2, %ecx ; movl %cs, %eax ; cmpl $0x33, %eax ; jne 1f
///   movl $3, %ecx ; movzwl 184(%rdx), %eax ; cmpl $0x23, %eax ; jne 1f
///   movl $4, %ecx ; cmpq $6, 200(%rdx) ; jne 1f
///   movl $1, %ecx
/// 1:
///   movl %ecx, 0x410f20 ; addq $2, 168(%rdx) ; ret
/// restorer:
///   movl $15, %eax ; syscall
///   .code32
/// code32:
///   movl 0x410f10, %eax ; movl %eax, %fs ; movl %fs:0, %eax
///   cmpl $0x57303634, %eax ; je 2f ; movl $1, 0x410f24
/// 2:
///   ud2
///   movl %cs, %eax ; cmpl $0x23, %eax ; jne bad64
///   pushl $0x33 ; call 4f
/// 4:
///   addl $(back64 - 4b), (%esp) ; lretl
///   .code64
/// bad64:
///   movl $1, 0x410f28 ; jmp back64
/// ```
///
/// Assembled by GNU `as --64`, linked at the image's entry, and read back
/// with `objcopy -O binary -j .text`.
const WOW64: &[u8] = &[
    0x48, 0x83, 0xec, 0x20, 0x48, 0x8d, 0x05, 0x64, 0x01, 0x00, 0x00, 0x48, 0x89, 0x04, 0x24, 0x48,
    0xc7, 0x44, 0x24, 0x08, 0x04, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05, 0x91, 0x01, 0x00, 0x00, 0x48,
    0x89, 0x44, 0x24, 0x10, 0x48, 0xc7, 0x44, 0x24, 0x18, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x0d, 0x00,
    0x00, 0x00, 0xbf, 0x04, 0x00, 0x00, 0x00, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0x41, 0xba, 0x08, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0xbf, 0x09, 0x00, 0x00, 0x00, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x16, 0x01,
    0x00, 0x00, 0xbb, 0x00, 0x0f, 0x41, 0x00, 0xc7, 0x03, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x43, 0x04,
    0x00, 0x00, 0x41, 0x00, 0xc7, 0x43, 0x08, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x43, 0x0c, 0x41, 0x00,
    0x00, 0x00, 0xb8, 0xf3, 0x00, 0x00, 0x00, 0xcd, 0x80, 0xbf, 0x0a, 0x00, 0x00, 0x00, 0x85, 0xc0,
    0x0f, 0x85, 0xe2, 0x00, 0x00, 0x00, 0x8b, 0x03, 0xc1, 0xe0, 0x03, 0x83, 0xc8, 0x03, 0x89, 0x04,
    0x25, 0x10, 0x0f, 0x41, 0x00, 0xc7, 0x04, 0x25, 0x00, 0x00, 0x41, 0x00, 0x34, 0x36, 0x30, 0x57,
    0x8e, 0xe0, 0xc7, 0x04, 0x25, 0x00, 0x01, 0x41, 0x00, 0xdf, 0x9b, 0x57, 0x13, 0xb8, 0x9e, 0x00,
    0x00, 0x00, 0xbf, 0x02, 0x10, 0x00, 0x00, 0xbe, 0x00, 0x01, 0x41, 0x00, 0x0f, 0x05, 0xbf, 0x0b,
    0x00, 0x00, 0x00, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x9c, 0x00, 0x00, 0x00, 0x8c, 0xe0, 0xbf, 0x0c,
    0x00, 0x00, 0x00, 0x66, 0x85, 0xc0, 0x0f, 0x85, 0x8c, 0x00, 0x00, 0x00, 0x64, 0x8b, 0x04, 0x25,
    0x00, 0x00, 0x00, 0x00, 0xbf, 0x0d, 0x00, 0x00, 0x00, 0x3d, 0xdf, 0x9b, 0x57, 0x13, 0x75, 0x78,
    0xb8, 0x2b, 0x00, 0x00, 0x00, 0x8e, 0xd8, 0x8e, 0xc0, 0x48, 0x89, 0x24, 0x25, 0x80, 0x0f, 0x41,
    0x00, 0xbc, 0x00, 0x0e, 0x41, 0x00, 0x6a, 0x23, 0x48, 0x8d, 0x05, 0xa8, 0x00, 0x00, 0x00, 0x50,
    0x48, 0xcb, 0x48, 0x8b, 0x24, 0x25, 0x80, 0x0f, 0x41, 0x00, 0x8b, 0x04, 0x25, 0x20, 0x0f, 0x41,
    0x00, 0xbf, 0x14, 0x00, 0x00, 0x00, 0x85, 0xc0, 0x74, 0x3e, 0xbf, 0x15, 0x00, 0x00, 0x00, 0x83,
    0xf8, 0x02, 0x74, 0x34, 0xbf, 0x16, 0x00, 0x00, 0x00, 0x83, 0xf8, 0x03, 0x74, 0x2a, 0xbf, 0x17,
    0x00, 0x00, 0x00, 0x83, 0xf8, 0x04, 0x74, 0x20, 0xbf, 0x18, 0x00, 0x00, 0x00, 0x83, 0x3c, 0x25,
    0x24, 0x0f, 0x41, 0x00, 0x00, 0x75, 0x11, 0xbf, 0x19, 0x00, 0x00, 0x00, 0x83, 0x3c, 0x25, 0x28,
    0x0f, 0x41, 0x00, 0x00, 0x75, 0x02, 0x31, 0xff, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb9,
    0x02, 0x00, 0x00, 0x00, 0x8c, 0xc8, 0x83, 0xf8, 0x33, 0x75, 0x25, 0xb9, 0x03, 0x00, 0x00, 0x00,
    0x0f, 0xb7, 0x82, 0xb8, 0x00, 0x00, 0x00, 0x83, 0xf8, 0x23, 0x75, 0x14, 0xb9, 0x04, 0x00, 0x00,
    0x00, 0x48, 0x83, 0xba, 0xc8, 0x00, 0x00, 0x00, 0x06, 0x75, 0x05, 0xb9, 0x01, 0x00, 0x00, 0x00,
    0x89, 0x0c, 0x25, 0x20, 0x0f, 0x41, 0x00, 0x48, 0x83, 0x82, 0xa8, 0x00, 0x00, 0x00, 0x02, 0xc3,
    0xb8, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xa1, 0x10, 0x0f, 0x41, 0x00, 0x8e, 0xe0, 0x64, 0xa1,
    0x00, 0x00, 0x00, 0x00, 0x3d, 0x34, 0x36, 0x30, 0x57, 0x74, 0x0a, 0xc7, 0x05, 0x24, 0x0f, 0x41,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x0b, 0x8c, 0xc8, 0x83, 0xf8, 0x23, 0x75, 0x0f, 0x6a, 0x33,
    0xe8, 0x00, 0x00, 0x00, 0x00, 0x81, 0x04, 0x24, 0x2d, 0xff, 0xff, 0xff, 0xcb, 0xc7, 0x04, 0x25,
    0x28, 0x0f, 0x41, 0x00, 0x01, 0x00, 0x00, 0x00, 0xe9, 0x15, 0xff, 0xff, 0xff,
];

/// [`WOW64`], and what its status says.
///
/// Verifies: `L.x86_64.42`, `L.x86_64.43`, `L.x86_64.61`
fn check_wow64() -> Result<(), &'static str> {
    match run_program(b"/wow64", WOW64)? {
        0 => {}
        9 => return Err("the WoW64 program's rt_sigaction failed"),
        10 => return Err("a 64-bit program's i386 set_thread_area through int $0x80 failed"),
        11 => return Err("the WoW64 program's ARCH_SET_FS failed"),
        12 => return Err("ARCH_SET_FS left a thread-local selector in FS"),
        13 => return Err("%fs:0 after ARCH_SET_FS did not read the base it set"),
        20 => return Err("a 64-bit handler for a fault in 32-bit code never ran"),
        21 => return Err("a 64-bit handler for a fault in 32-bit code ran in 32-bit mode"),
        22 => {
            return Err(
                "a 64-bit handler's sigcontext for a fault in 32-bit code did not say code \
                 selector 0x23",
            );
        }
        23 => {
            return Err(
                "a 64-bit handler's sigcontext for a ud2 in 32-bit code did not say trap 6",
            );
        }
        24 => {
            return Err(
                "32-bit code did not read its block through the selector set_thread_area gave",
            );
        }
        25 => {
            return Err(
                "rt_sigreturn through a 64-bit frame saying 0x23 did not resume compatibility mode",
            );
        }
        _ => {
            return Err(
                "a fault in a 64-bit program's 32-bit code did not reach its 64-bit handler on a \
                 64-bit frame",
            );
        }
    }
    println!(
        "  wow64    a 64-bit program's 32-bit code read its block through an int $0x80 \
         set_thread_area selector, its ud2 reached the 64-bit SIGILL handler on a 64-bit frame \
         saying cs 0x23 and trap 6, and rt_sigreturn resumed it in compatibility mode; \
         ARCH_SET_FS left FS null and its base read back"
    );
    Ok(())
}

/// A 64-bit program whose `SA_SIGINFO` handler for `SIGUSR1` forges two
/// eight-byte fields of its own `ucontext` before returning through
/// `rt_sigreturn`: the 64-bit twin of [`FORGE_I386`]. The handler's two
/// `movq $VALUE, FIELD(%rdx)` are patched per case at [`FORGE_64_VALUE_AT`]
/// and [`FORGE_64_FIELD_AT`], and at [`FORGE_64_VALUE2_AT`] and
/// [`FORGE_64_FIELD2_AT`]; a case that forges one field writes it twice. After the return the program exits 3 if
/// `IOPL` is set, 4 if `IF` is clear, 5 if `CS` is not `0x33`, 6 if `SS` is
/// not `0x2b`, and 0 otherwise; 9 is a failed `rt_sigaction`.
///
/// ```text
///   subq $32, %rsp
///   leaq handler(%rip), %rax ; movq %rax, (%rsp)
///   movq $0x04000004, 8(%rsp)
///   leaq restorer(%rip), %rax ; movq %rax, 16(%rsp)
///   movq $0, 24(%rsp)
///   movl $13, %eax ; movl $10, %edi ; movq %rsp, %rsi ; xorl %edx, %edx
///   movl $8, %r10d ; syscall
///   movl $9, %edi ; testq %rax, %rax ; jnz exit
///   movl $39, %eax ; syscall
///   movl %eax, %edi ; movl $10, %esi ; movl $62, %eax ; syscall
///   pushfq ; popq %rax
///   movl $3, %edi ; testl $0x3000, %eax ; jnz exit
///   movl $4, %edi ; testl $0x200, %eax ; jz exit
///   movl %cs, %eax ; movl $5, %edi ; cmpl $0x33, %eax ; jne exit
///   movl %ss, %eax ; movl $6, %edi ; cmpl $0x2b, %eax ; jne exit
///   xorl %edi, %edi
/// exit:
///   movl $231, %eax ; syscall
/// handler:
///   movabsq $0x1122334455667788, %rax ; movq %rax, 0x7eadbeef(%rdx)
///   movabsq $0x99aabbccddeeff00, %rax ; movq %rax, 0x7ecafe01(%rdx) ; ret
/// restorer:
///   movl $15, %eax ; syscall
/// ```
///
/// Assembled by GNU `as --64` with those placeholders, linked at the image's
/// entry, and read back with `objcopy -O binary -j .text`.
const FORGE_64: &[u8] = &[
    0x48, 0x83, 0xec, 0x20, 0x48, 0x8d, 0x05, 0x93, 0x00, 0x00, 0x00, 0x48, 0x89, 0x04, 0x24, 0x48,
    0xc7, 0x44, 0x24, 0x08, 0x04, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05, 0xa2, 0x00, 0x00, 0x00, 0x48,
    0x89, 0x44, 0x24, 0x10, 0x48, 0xc7, 0x44, 0x24, 0x18, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x0d, 0x00,
    0x00, 0x00, 0xbf, 0x0a, 0x00, 0x00, 0x00, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0x41, 0xba, 0x08, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0xbf, 0x09, 0x00, 0x00, 0x00, 0x48, 0x85, 0xc0, 0x75, 0x49, 0xb8, 0x27,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x89, 0xc7, 0xbe, 0x0a, 0x00, 0x00, 0x00, 0xb8, 0x3e, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0x9c, 0x58, 0xbf, 0x03, 0x00, 0x00, 0x00, 0xa9, 0x00, 0x30, 0x00, 0x00, 0x75,
    0x26, 0xbf, 0x04, 0x00, 0x00, 0x00, 0xa9, 0x00, 0x02, 0x00, 0x00, 0x74, 0x1a, 0x8c, 0xc8, 0xbf,
    0x05, 0x00, 0x00, 0x00, 0x83, 0xf8, 0x33, 0x75, 0x0e, 0x8c, 0xd0, 0xbf, 0x06, 0x00, 0x00, 0x00,
    0x83, 0xf8, 0x2b, 0x75, 0x02, 0x31, 0xff, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0xb8,
    0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x48, 0x89, 0x82, 0xef, 0xbe, 0xad, 0x7e, 0x48,
    0xb8, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x48, 0x89, 0x82, 0x01, 0xfe, 0xca, 0x7e,
    0xc3, 0xb8, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// Where [`FORGE_64`]'s handler keeps the value it writes, the
/// `0x1122334455667788` placeholder.
const FORGE_64_VALUE_AT: usize = 160;

/// Where it keeps the field's offset in the `ucontext`, the `0x7eadbeef`
/// placeholder.
const FORGE_64_FIELD_AT: usize = 171;

/// The second write's value, the `0x99aabbccddeeff00` placeholder.
const FORGE_64_VALUE2_AT: usize = 177;

/// The second write's field, the `0x7ecafe01` placeholder.
const FORGE_64_FIELD2_AT: usize = 188;

/// [`FORGE_64`] once for each case: a kernel-half `RIP` and a kernel-half
/// `RSP` must end the program with `SIGSEGV`, and so must 32-bit user code
/// with a `RIP` of 4 GiB, past the 32-bit code segment's limit, which the
/// kernel's `IRETQ` would fault on in ring 0; flags asking `IOPL` 3 and the
/// trap flag with `IF` clear, and `CS` and `SS` `0x10` and `0x18`, must
/// resume it in ring 3 with `IOPL` 0, `IF` set and its own selectors; and a
/// frame changed only in `RAX` must return as it was.
///
/// Verifies: `L.x86_64.43`
fn check_forged_64_bit_frames() -> Result<(), &'static str> {
    /// `uc_mcontext` in the `ucontext`, and the fields in it.
    const MCONTEXT: u32 = 40;
    const KERNEL_HALF: u64 = 0xffff_8000_0000_0000;
    /// `RIP` and the selectors' eight bytes in `uc_mcontext`.
    const RIP: u32 = MCONTEXT + 128;
    const SELECTORS: u32 = MCONTEXT + 144;
    let cases: [(u32, u64, u32, u64, i32, &'static str); 6] = [
        (
            RIP,
            KERNEL_HALF,
            RIP,
            KERNEL_HALF,
            killed_by(SIGSEGV),
            "a 64-bit frame with a kernel-half RIP did not end the program with SIGSEGV",
        ),
        (
            MCONTEXT + 120,
            KERNEL_HALF,
            MCONTEXT + 120,
            KERNEL_HALF,
            killed_by(SIGSEGV),
            "a 64-bit frame with a kernel-half RSP did not end the program with SIGSEGV",
        ),
        (
            MCONTEXT + 136,
            0x3100,
            MCONTEXT + 136,
            0x3100,
            0,
            "a 64-bit frame asking IOPL 3, TF and IF clear did not resume with IOPL 0 and IF set",
        ),
        (
            SELECTORS,
            0x0018_0000_0000_0010,
            SELECTORS,
            0x0018_0000_0000_0010,
            0,
            "a 64-bit frame asking CS 0x10 and SS 0x18 did not resume with ring 3's selectors",
        ),
        (
            MCONTEXT + 104,
            0,
            MCONTEXT + 104,
            0,
            0,
            "a 64-bit frame changed only in RAX did not return",
        ),
        (
            RIP,
            0x1_0000_0000,
            SELECTORS,
            0x002b_0000_0000_0023,
            killed_by(SIGSEGV),
            "a 64-bit frame with CS 0x23 and a RIP of 4 GiB did not end the program with SIGSEGV",
        ),
    ];
    for (field, value, field2, value2, expected, problem) in cases {
        /// What a placeholder outside the program says.
        const SHORT: &str = "the 64-bit forging program is shorter than its placeholders' places";
        let mut code = FORGE_64.to_vec();
        code.get_mut(FORGE_64_VALUE_AT..FORGE_64_VALUE_AT + 8)
            .ok_or(SHORT)?
            .copy_from_slice(&value.to_le_bytes());
        code.get_mut(FORGE_64_FIELD_AT..FORGE_64_FIELD_AT + 4)
            .ok_or(SHORT)?
            .copy_from_slice(&field.to_le_bytes());
        code.get_mut(FORGE_64_VALUE2_AT..FORGE_64_VALUE2_AT + 8)
            .ok_or(SHORT)?
            .copy_from_slice(&value2.to_le_bytes());
        code.get_mut(FORGE_64_FIELD2_AT..FORGE_64_FIELD2_AT + 4)
            .ok_or(SHORT)?
            .copy_from_slice(&field2.to_le_bytes());
        let status = run_program(b"/forge64", &code)?;
        if status != expected {
            println!("  signal   the 64-bit forging program ended with {status}, not {expected}");
            return Err(problem);
        }
    }
    println!(
        "  signal   rt_sigreturn ended programs whose 64-bit frames held a kernel-half RIP or RSP, \
         or CS 0x23 with a RIP of 4 GiB, with SIGSEGV, resumed IOPL 3, TF and IF clear as IOPL 0 with IF set and CS and SS 0x10 \
         and 0x18 as 0x33 and 0x2b, and took an honest change"
    );
    Ok(())
}

/// Show that the exceptions nothing masks are survived where they land: an NMI
/// in the kernel, and hardware breakpoints in the `SYSCALL` trampoline's ring-0
/// stretches on the program's stack and `GS`. See `paranoid`. Then the
/// exceptions a program raises itself, each of which has to end it with the
/// signal Linux gives it, and `arch_prctl`'s refusals, the one call this
/// entry answers itself. See `trap::check`; and `syscall::check` for the
/// registers a fork child is handed.
///
/// # Errors
///
/// What did not come back, or did not end as it had to.
pub fn check_exception_entry() -> Result<(), &'static str> {
    crate::arch::x86_64::paranoid::check::run()?;
    ferrix_kernel_core::arch::x86_64::syscall::check::run()?;
    ferrix_kernel_core::arch::x86_64::syscall::check::run_decode()?;
    ferrix_kernel_core::arch::x86_64::gdt::check::run()?;
    run()
}
