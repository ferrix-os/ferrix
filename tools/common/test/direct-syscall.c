/*
 * direct-syscall.exe: a Windows system call made by a program's own code,
 * as Arknights: Endfield's anti-cheat makes them, for Syscall User Dispatch
 * (docs/SECCOMP.md section 13, the design's 7.4).
 *
 * It reads NtQueryInformationProcess's number from ntdll's own stub
 * (`mov r10, rcx; mov eax, <n>`), then runs a `syscall` with that number
 * from its own .text for ProcessBasicInformation, and compares the answer
 * with ntdll's. Under Wine the direct call reaches the kernel from outside
 * the range Wine names, so it must come back to Wine as a SIGSYS and be
 * performed there; a kernel without dispatch runs it as an unrelated Linux
 * call. Prints `DIRECT-SYSCALL PASS` or `DIRECT-SYSCALL FAIL: ...` and
 * exits 0 or 1.
 *
 * Built with x86_64-w64-mingw32-gcc -O2 -o direct-syscall.exe
 * direct-syscall.c -lntdll.
 */
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <string.h>

/* The direct call: Windows x64's convention, number in eax, first argument
 * moved to r10, the rest where the caller put them: the fifth at 0x28(%rsp),
 * where the kernel side reads it, and the number, the sixth, at 0x30(%rsp). */
__asm__(
    ".text\n"
    ".globl direct_call\n"
    "direct_call:\n"
    "    mov %rcx, %r10\n"
    "    mov 0x30(%rsp), %eax\n"
    "    syscall\n"
    "    ret\n");
extern NTSTATUS direct_call(HANDLE process, ULONG class, PVOID info, ULONG length,
                            PULONG returned, ULONG number);

int main(void)
{
    HMODULE ntdll = GetModuleHandleA("ntdll.dll");
    const unsigned char *stub =
        ntdll ? (const unsigned char *)GetProcAddress(ntdll, "NtQueryInformationProcess") : NULL;
    if (!stub) {
        printf("DIRECT-SYSCALL FAIL: no NtQueryInformationProcess in ntdll\n");
        return 1;
    }
    /* 4c 8b d1 = mov r10, rcx; b8 = mov eax, imm32. */
    if (stub[0] != 0x4c || stub[1] != 0x8b || stub[2] != 0xd1 || stub[3] != 0xb8) {
        printf("DIRECT-SYSCALL FAIL: ntdll's stub starts %02x %02x %02x %02x, not "
               "mov r10, rcx; mov eax\n", stub[0], stub[1], stub[2], stub[3]);
        return 1;
    }
    ULONG number;
    memcpy(&number, stub + 4, sizeof number);

    PROCESS_BASIC_INFORMATION by_ntdll, direct;
    ULONG got = 0;
    memset(&by_ntdll, 0, sizeof by_ntdll);
    memset(&direct, 0xaa, sizeof direct);
    NTSTATUS status = NtQueryInformationProcess(GetCurrentProcess(), ProcessBasicInformation,
                                                &by_ntdll, sizeof by_ntdll, &got);
    if (status != 0) {
        printf("DIRECT-SYSCALL FAIL: ntdll's own call answered %#lx\n", (unsigned long)status);
        return 1;
    }
    got = 0;
    status = direct_call(GetCurrentProcess(), ProcessBasicInformation, &direct, sizeof direct,
                         &got, number);
    if (status != 0) {
        printf("DIRECT-SYSCALL FAIL: the direct call (number %#lx) answered %#lx\n",
               (unsigned long)number, (unsigned long)status);
        return 1;
    }
    if (direct.UniqueProcessId != by_ntdll.UniqueProcessId ||
        direct.PebBaseAddress != by_ntdll.PebBaseAddress) {
        printf("DIRECT-SYSCALL FAIL: the direct call's answer differs from ntdll's\n");
        return 1;
    }
    printf("DIRECT-SYSCALL PASS (number %#lx, pid %lu)\n", (unsigned long)number,
           (unsigned long)(ULONG_PTR)by_ntdll.UniqueProcessId);
    return 0;
}
