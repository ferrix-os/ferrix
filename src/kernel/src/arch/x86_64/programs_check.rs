//! The little program K15's check runs (`syscall/build_check.rs`).

// CHECKS-ONLY: syscall/build_check.rs
/// Exits with its stack's page number: `movq %rsp, %rdi; shrq $12, %rdi; movl $231, %eax; syscall`.
pub const USER_STACK_PROGRAM: &[u8] = &[
    0x48, 0x89, 0xe7, 0x48, 0xc1, 0xef, 0x0c, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05,
];
