/*
 * The retpoline thunks nv-kernel.o calls: it is built with
 * -mindirect-branch=thunk-extern, as Linux's kernel is, and Linux supplies
 * __x86_indirect_thunk_<reg>. nvrm is a user process, where the kernel's
 * own mitigations stand between processes; each thunk is the plain
 * indirect jump that a retpoline-free build would have made.
 *
 * SPDX-License-Identifier: MIT
 */
	.text
.macro THUNK reg
	.globl __x86_indirect_thunk_\reg
	.type __x86_indirect_thunk_\reg, @function
__x86_indirect_thunk_\reg:
	jmp *%\reg
	.size __x86_indirect_thunk_\reg, . - __x86_indirect_thunk_\reg
.endm
	THUNK rax
	THUNK rbx
	THUNK rcx
	THUNK rdx
	THUNK rsi
	THUNK r8
	THUNK r9
	THUNK r10
	THUNK r11
	THUNK r12
	THUNK r13
	THUNK r14
	THUNK r15
	.section .note.GNU-stack,"",@progbits
