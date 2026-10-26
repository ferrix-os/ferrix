/* Included before seccomp_bpf.c by Ferrix's musl build (tools/common/fetch/fetch-seccomp-selftest.sh): musl has SIGSYS in
 * siginfo_t (what glibc 2.26 added), and its <sys/prctl.h> already declares
 * what <linux/prctl.h> would declare again. */
#define __GLIBC_PREREQ(major, minor) 1
#define _LINUX_PRCTL_H
/* musl's 32-bit Arm headers name only the 64-bit-time clock_nanosleep; the
 * test asks for the call by the number Linux's arm table gives it. */
#if defined(__arm__)
#include <asm/unistd.h>
#ifndef __NR_clock_nanosleep
#define __NR_clock_nanosleep 265
#endif
#endif
