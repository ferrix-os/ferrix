# Ferrousli against POSIX.1-2024

POSIX.1-2024 is a goal by the customer's decision of 2026-09-13, on the
condition that it never breaks Linux compatibility: where the two differ, Linux
wins (`docs/BACKLOG.md`, Decisions). Ferrix takes POSIX through its C library
over the Linux ABI, so the library half of that goal is `src/user/system/linux/ferrousli/`'s. This
file measures it, one interface at a time, against the standard's own list of
system interfaces, and prices what is missing in story points.

It was measured on 2026-09-14 at `develop` 55753ea, whose `src/user/system/linux/ferrousli/` is
unchanged since 5e9b0b6. Each landing that closes part of the gap updates the
tables below in the same commit, the way a stage's roadmap section is updated.
Measured again on 2026-09-30 at `main` 7c9df826, the same way (§ *How it was
measured*): 29 names had landed without moving here -- `posix_spawn` and 20 of
its family, `pthread_atfork`, `setresuid` and `setresgid`, `thrd_equal` as a
function, and `dlopen`, `dlsym`, `dlclose` and `dlerror` -- and the count and
area tables had fallen behind the index, which they are recounted from now.

## The count

The standard's index lists 1243 interfaces: functions, and the macros and
variables it specifies beside them.

| Status | Interfaces | Meaning |
|---|---|---|
| present | 1137 | defined by `libferrousli.a`, or a macro the standard specifies as one and `include/` defines |
| macro only | 2 | a function the standard requires, which the header defines only as a macro: a call works, taking its address or `#undef` does not |
| broken | 2 | present, but it fails to link or gives a wrong answer |
| stubbed | 0 | a stand-in that ends the program; the last left `src/stubs.rs` on 2026-09-16, and the file is gone |
| absent | 102 | not there |

106 interfaces are missing in one of the last four ways. None of them is
written on a branch any more: the three unlanded branches of 2026-09-13,
`ferrousli-math`, `ferrousli-threads` and `ferrousli-misc`, were built,
fixed and landed on 2026-09-16.

## What is left, by area, in points

Areas group headers by what implements them. Points follow
`docs/BACKLOG.md`: 1 is a change whose pattern and tests already exist, 13 a
new subsystem. Every area's missing names are in the index at the end.

| Area | Interfaces | Missing | On a branch | Points | What the points buy |
|---|---|---|---|---|---|
| Language support and the standard library | 118 | 0 | 0 | 0 | landed: `quick_exit` and `at_quick_exit`; `a64l`, `l64a`, `getsubopt`, `secure_getenv`; `setkey`, with `encrypt` below it |
| Strings and characters | 72 | 0 | 0 | 0 | landed: `strcasecmp_l`, `strncasecmp_l` |
| Wide and multibyte characters | 118 | 0 | 0 | 0 | landed: the `wscanf` family, `fwscanf` to `wscanf`; `open_wmemstream`; the `wprintf` family, `fwprintf` to `wprintf`; wide-character stream I/O (`fgetwc`, `fgetws`, `fputwc`, `fputws`, `getwc`, `getwchar`, `putwc`, `putwchar`, `ungetwc`, `fwide`); the `wcstol` and `wcstod` families with `wcstoimax` and `wcstoumax`; `wcsftime`, `wcslcpy`, `wcslcat` |
| Standard I/O | 70 | 0 | 0 | 0 | landed: `tmpnam` |
| Math and the floating-point environment | 201 | 28 | 0 | 5 | the 28 `long double` forms the x87 has no single instruction for -- the transcendentals, `powl` and `logl` among them as `double` stand-ins, `cbrtl`, `hypotl`, `fmal` and the gamma and error functions -- each a port of musl's, 5. Landed: every `double` and `float` function: the error and gamma functions with `signgam` and `lgamma_r`, the Bessel functions; the hyperbolic functions and `hypot` for `double` and `float`; `tan`, `asin`, `acos`, `atan` for `double`, and all of them with `sin`, `cos` and `atan2` for `float`; `exp2`, `expm1`, `log2`, `log10`, `log1p` for `double` and `float`, with `expf`, `logf` and `powf`; `fenv.h`; rounding, manipulation, remainders and `fma` for `double` and `float`; `sin`, `cos`, `exp`, `log`, `pow` and `atan2` for `double`, bit for bit musl's in every rounding mode; and the classifiers for all three types; the 31 `long double` forms the x87 computes directly, through naked shims for the calling convention no Rust signature can say: `fabsl`, `copysignl`, `sqrtl`, the rounding family, the remainder family and the manipulation family |
| Complex arithmetic | 69 | 22 | 0 | 2 | the `long double complex` forms, after the rest of `long double` math 2. Landed: every `double complex` and `float complex` function, `creal` and `cimag` among them as functions, bit for bit musl's |
| Locales, messages and conversion | 32 | 3 | 0 | 8 | reading `.mo` catalogues for the `gettext` family 3; `iconv`'s other character sets and glibc's transliteration 2; `strfmon`, `strfmon_l` 2; `getlocalename_l` 1. Landed: the `gettext` family with the answers glibc gives without a catalogue, and `iconv` between UTF-8, UTF-16, UTF-32, UCS-2, UCS-4, ASCII, ISO-8859-1 and CP1252, for GLib (`docs/CHROME.md`); `catopen`, `catgets`, `catclose` |
| Files, directories and I/O multiplexing | 46 | 1 | 0 | 1 | `posix_getdents` |
| Processes, identity and the system | 103 | 4 | 0 | 4 | `confstr` 1; `nice` 1; `posix_close` 1; `fmtmsg` 1. Landed: `setresuid`, `setresgid`; `lockf`, as musl's over `fcntl` record locks, for CUPS (`docs/CHROME.md` §8); `pathconf`, `fpathconf`; `encrypt` and `setkey`, DES through the bit-array interface, checked against FIPS 46-3's own vector |
| Spawning | 25 | 4 | 0 | 3 | `posix_spawn_file_actions_addchdir` and `posix_spawn_file_actions_addfchdir`, which `include/` does not declare, 1; `_Fork` 1; `fexecve` 1. Landed: `posix_spawn`, `posix_spawnp` and the rest of their file actions and attributes |
| Signals and non-local jumps | 28 | 4 | 0 | 2 | `psignal`, `psiginfo` 1; `sig2str`, `str2sig` 1 |
| Time and clocks | 29 | 3 | 0 | 3 | `getdate` and `getdate_err` 2; `timespec_get` 1 |
| Threads and scheduling | 145 | 4 | 0 | 2 | the `clock` variants of the condition, mutex and read-write lock waits 2. Landed: `pthread_atfork`; `thrd_equal` as a function as well as a macro; cancellation, `pthread_cancel` and `pthread_testcancel` with their cancellation points |
| Memory mapping and System V IPC | 23 | 0 | 0 | 0 | landed: `ftok`, for NSPR (`docs/CHROME.md`) |
| Realtime: asynchronous I/O, message queues, timers, shared memory | 27 | 20 | 0 | 10 | `aio.h` over threads 3; the rest of `mqueue.h`, `mq_open` to `mq_notify`, 3; `SIGEV_THREAD` for `timer_create`, which refuses it with `ENOTSUP`, 3; `clock_getcpuclockid` 1. Landed: `timer_create`, `timer_settime`, `timer_gettime`, `timer_getoverrun` and `timer_delete`, each its system call, for Claude Code on the `--everything` desktop (`docs/CLAUDE-CODE.md` §5; Ferrix's kernel answers them `ENOSYS` today, as it does glibc's); `shm_open` and `shm_unlink`, for LLVM (`docs/roadmap/chrome.md`). Typed memory (`posix_typed_mem_*`, `posix_mem_offset`) is the TYM option, which ferrousli does not claim: 0 |
| Terminals and devices | 25 | 2 | 0 | 2 | `ctermid` 1; `posix_devctl` and `<devctl.h>` 1. Landed: `posix_openpt`, `grantpt`, `unlockpt`, `ptsname`, `ptsname_r`, for foot (`docs/CHROME.md`) |
| Networking and name resolution | 55 | 0 | 0 | 0 | landed: `getaddrinfo`, `getnameinfo`, `freeaddrinfo` and `gai_strerror` over `/etc/hosts` and a DNS stub resolver; the hosts, networks, protocols and services databases; `if_nameindex`, `if_freenameindex`, `if_indextoname`; `in6addr_any`, `in6addr_loopback`, `sockatmark` |
| Patterns, paths and search | 23 | 2 | 0 | 3 | `wordexp` 3. Landed: `nftw`, musl's with glibc's type flags and `FTW_ACTIONRETVAL`, for GLib (`docs/CHROME.md`); `glob` and `globfree`; `search.h`'s hash table, trees, linear search and queues; `libgen.h`'s `basename` and `dirname`, and `regex.h`, replacing five stubs |
| Users, groups and databases | 29 | 9 | 0 | 3 | `<ndbm.h>` and the `dbm_*` functions |
| Dynamic loading | 5 | 0 | 0 | 0 | landed: `dladdr`, over the program's own headers; `dlopen`, `dlsym`, `dlclose` and `dlerror`, which load shared objects when `ld-ferrousli` started the program and, in a static program, return null with `dlerror` saying why, as musl's do |
| Across areas | | | | 3 | POSIX.1-2024's declarations in `include/` 3 |
| **All** | **1243** | **106** | **0** | **51** | |

## Present but broken

Two are: `powl`, which Chrome imports, is `pow` of its arguments rounded to
`double` and widened back, as musl does for AArch64's binary128 `long double`,
until musl's x87 `powl` is ported; and `logl`, which LLVM imports, is `log`
the same way. Their results carry 53 bits, not 64.

This list held interfaces that looked present and were not,
which made them worse than a missing name: a program compiled against the
header and then failed to link, or ran and got the wrong answer.

* `assert` called `__assert_fail`, which was not defined. It is now, and writes
  musl's message.
* `crypt` gave `"*"` for the two-character salt POSIX requires it to take. It
  computes the traditional DES hash now, ported from musl's `crypt_des.c`;
  `$2*$` blowfish still gives `"*"`.
* `fpclassify` for every type, and `isinf`, `isnan`, `isnormal`, `isfinite`,
  `signbit` and `isgreater` to `isunordered` for `long double`, called helpers
  that were not defined. `__fpclassify`, `__fpclassifyf`, `__fpclassifyl`,
  `__signbit`, `__signbitf` and `__signbitl` are defined now, the `long double`
  ones reading the x87 value as musl does, unnormals classifying as NaN.

## Where Ferrix differs from Linux

**Credentials are per process.** Linux keeps user and group ids per thread in
the kernel (setuid(2), "C library/kernel differences"). POSIX requires every
thread of a process to share them, and glibc's and musl's `set*id` wrappers
emulate that by making every thread repeat the call. Ferrix's kernel keeps
`Credentials` on the `Process` (`src/kernel/src/syscall/process.rs`), which is what
POSIX requires, so on Ferrix one thread's `setuid` changes them all without the
broadcast. The product owner's ruling of 2026-09-14 is to keep it: no kernel
change. What follows from it:

* A program that relies on per-thread credentials through the raw system call,
  such as a file server that gives each thread its own ids, sees a difference on
  Ferrix.
* `ferrousli` on a Linux host does not conform until its `set*id` functions
  broadcast to every thread as NPTL's do. That broadcast is harmless on Ferrix,
  where each repeat sets ids the process already has.
* musl's broadcast is proven safe on Ferrix by `cargo xtask test-threads`
  since 2026-10-12: two threads survive root's `setuid(1000)`, whose repeat
  on the second thread asks for the 1000 the process already has. musl kills
  the process when a repeat fails, so the step holds only while setting an id
  to a current one stays permitted without privilege.

## Headers

`include/` holds musl 1.2.5's headers, unmodified but for five edits each
marked as ferrousli's (`src/user/system/linux/ferrousli/include/README.md`), and musl
1.2.5 predates POSIX.1-2024.

* **Missing:** `<devctl.h>` and `<ndbm.h>`.
* **Not declared anywhere in `include/`**, beyond those headers' contents:
  `posix_getdents`; the six `gettext` `_l` forms and `getlocalename_l`; the
  `clock` waits `pthread_cond_clockwait`, `pthread_mutex_clocklock`,
  `pthread_rwlock_clockrdlock`, `pthread_rwlock_clockwrlock` and
  `sem_clockwait`; `sig2str` and `str2sig`;
  `posix_spawn_file_actions_addchdir` and `posix_spawn_file_actions_addfchdir`;
  the typed-memory functions. `wcslcpy` and `wcslcat` are declared in
  `<wchar.h>` now, an edit that says it is ferrousli's.
* **Not checked:** the constants, types and structure members POSIX.1-2024
  adds, such as `O_CLOFORK`, `FD_CLOFORK` and `SOCK_CLOFORK`. They are part of
  the 3 points for declarations.

## What this does not measure

* **Behaviour.** An interface counts as present when the archive defines it.
  Whether it does what the standard says is libc-test's to measure, the P2
  row in `docs/BACKLOG.md`.
* **Anything but x86-64** and static linking, the only configuration ferrousli
  builds today.
* **The utilities and the shell**, the other half of POSIX, which busybox and
  the kernel answer, not the library.
* **The kernel's side.** Each interface needs Linux system calls answered as
  Linux answers them. The POSIX.1-2024 interface sweep, a P1 row, measures
  that.

## Beyond POSIX: what relibc has

For comparison, os-08 checked relibc (master 1490c4d) the same way: every
`extern "C"` function outside its Redox-only files against the same archive.
relibc had 398 functions ferrousli lacked when that was measured. Most are in
the tables above. These are the ones POSIX does not require, and several are
what real programs call. The `netdb` and `ifaddrs` rows — `gethostbyname`,
`gethostbyaddr`, `hstrerror`, `herror`, `__h_errno_location`, `getifaddrs` and
`freeifaddrs` — left the list with the name-resolution landing of 2026-09-16,
which added `ether_*`, `res_*` and the `ns_*` parser beside them, the
`sys/epoll` row with the epoll and eventfd wrappers of 2026-09-17, and `utime`
with the git port:

| Area | Functions |
|---|---|
| pthread | `pthread_getconcurrency`, `pthread_setconcurrency` |
| unistd | `brk`, `getpass`, `getwd`, `sbrk`, `ualarm` |
| stdlib | `ecvt`, `fcvt`, `gcvt`, `ttyslot` |
| err | `err`, `err_set_exit`, `err_set_file`, `errc`, `errx`, `verr`, `verrc`, `verrx`, `vwarn`, `vwarnc`, `vwarnx`, `warn`, `warnc`, `warnx` |
| stdio | `cuserid`, `gets`, `renameat2`, `tempnam` |
| time | `timelocal`, `timespec_getres` |
| arpa/inet | `inet_lnaof`, `inet_makeaddr`, `inet_netof`, `inet_network` |
| shadow | `endspent`, `getspent`, `getspnam`, `setspent` |
| dirent | `fdclosedir` |
| pty | `forkpty`, `openpty` |
| dl-tls | `__tls_get_addr` |
| float | `flt_rounds` |
| sgtty | `gtty` |
| string | `strnlen_s` |
| sys/timeb | `ftime` |
| utmp | `login_tty` |

Ferrousli in turn exports about 170 functions relibc lacks, mostly glibc's
binary names (`__xstat`, the `*64` names, `__isoc23_`, `__*_chk`,
`__libc_start_main`) and Linux's own calls.

## How it was measured

1. **The standard's list.** The system interfaces index of POSIX.1-2024,
   `https://pubs.opengroup.org/onlinepubs/9799919799/idx/functions.html`, gives
   1243 names. Each name's own page gives its header: the last `#include` before
   its declaration in the SYNOPSIS, skipping includes marked `[OH]`. It also
   gives its option codes: the margin codes around the declaration, shown in the
   index without `CX` and `OH`. `_Exit.html` and `_exit.html` are one file on a
   case-insensitive disk, so those two headers were set by hand.
2. **The library's list.** `nm -g --defined-only` over
   `src/user/system/linux/ferrousli/target/x86_64-unknown-linux-gnu/release/libferrousli.a`, built
   from 5e9b0b6, minus the 30 names in `src/stubs.rs`. Sort both lists with
   `LC_ALL=C` before comparing them.
3. **Macros.** A name counts as present through `include/` only when the
   standard specifies it as a macro (`FD_SET`, `va_arg`, `errno`, `isgreater`
   and its five siblings, `CMPLX`, `pthread_cleanup_push`). A required function
   that its own header defines only as a macro is "macro only". A macro in
   `<tgmath.h>` counts for nothing. The macros that expand to helpers were read
   in musl's headers, and a macro whose helper is missing is "broken" even when
   the standard specifies it as a macro: that is how the link-breakers above
   were found.
4. **Branches.** A missing name is "written on" a branch when that branch's
   diff from `develop` adds an `extern "C" fn` of that name.

## Index

Every interface, by area and header. Option codes follow a name in
parentheses: `XSI` is the X/Open System Interfaces option; `SPN`, `MSG`, `PS`,
`TPS`, `TSH`, `TYM`, `SHM`, `ADV`, `ML`, `MLR`, `MC1`, `IP6`, `TCT`, `CPT`,
`FSC`, `SIO` and `DC` are the other options; `OB` marks an obsolescent
interface.

### Language support and the standard library

| Header | Status | Interfaces |
|---|---|---|
| `<assert.h>` | present (1) | `assert` |
| `<endian.h>` | present (12) | `be16toh`, `be32toh`, `be64toh`, `htobe16`, `htobe32`, `htobe64`, `htole16`, `htole32`, `htole64`, `le16toh`, `le32toh`, `le64toh` |
| `<errno.h>` | present (1) | `errno` |
| `<inttypes.h>` | present (4) | `imaxabs`, `imaxdiv`, `strtoimax`, `strtoumax` |
| `<stdarg.h>` | present (4) | `va_arg`, `va_copy`, `va_end`, `va_start` |
| `<stdatomic.h>` | present (29) | `atomic_compare_exchange_strong`, `atomic_compare_exchange_strong_explicit`, `atomic_compare_exchange_weak`, `atomic_compare_exchange_weak_explicit`, `atomic_exchange`, `atomic_exchange_explicit`, `atomic_fetch_add`, `atomic_fetch_add_explicit`, `atomic_fetch_and`, `atomic_fetch_and_explicit`, `atomic_fetch_or`, `atomic_fetch_or_explicit`, `atomic_fetch_sub`, `atomic_fetch_sub_explicit`, `atomic_fetch_xor`, `atomic_fetch_xor_explicit`, `atomic_flag_clear`, `atomic_flag_clear_explicit`, `atomic_flag_test_and_set`, `atomic_flag_test_and_set_explicit`, `atomic_init`, `atomic_is_lock_free`, `atomic_load`, `atomic_load_explicit`, `atomic_signal_fence`, `atomic_store`, `atomic_store_explicit`, `atomic_thread_fence`, `kill_dependency` |
| `<stdlib.h>` | present (67) | `_Exit`, `a64l` (XSI), `abort`, `abs`, `aligned_alloc`, `at_quick_exit`, `atexit`, `atof`, `atoi`, `atol`, `atoll`, `bsearch`, `calloc`, `div`, `drand48` (XSI), `erand48` (XSI), `exit`, `free`, `getenv`, `getsubopt`, `initstate` (XSI), `jrand48` (XSI), `l64a` (XSI), `labs`, `lcong48` (XSI), `ldiv`, `llabs`, `lldiv`, `lrand48` (XSI), `malloc`, `mblen`, `mbstowcs`, `mbtowc`, `mkdtemp`, `mkostemp`, `mkstemp`, `mrand48` (XSI), `nrand48` (XSI), `posix_memalign`, `putenv` (XSI), `qsort`, `qsort_r`, `quick_exit`, `rand`, `random` (XSI), `realloc`, `reallocarray`, `realpath`, `secure_getenv`, `seed48` (XSI), `setenv`, `setkey` (XSI), `setstate` (XSI), `srand`, `srand48` (XSI), `srandom` (XSI), `strtod`, `strtof`, `strtol`, `strtold`, `strtoll`, `strtoul`, `strtoull`, `system`, `unsetenv`, `wcstombs`, `wctomb` |

### Strings and characters

| Header | Status | Interfaces |
|---|---|---|
| `<ctype.h>` | present (28) | `isalnum`, `isalnum_l`, `isalpha`, `isalpha_l`, `isblank`, `isblank_l`, `iscntrl`, `iscntrl_l`, `isdigit`, `isdigit_l`, `isgraph`, `isgraph_l`, `islower`, `islower_l`, `isprint`, `isprint_l`, `ispunct`, `ispunct_l`, `isspace`, `isspace_l`, `isupper`, `isupper_l`, `isxdigit`, `isxdigit_l`, `tolower`, `tolower_l`, `toupper`, `toupper_l` |
| `<string.h>` | present (37) | `memccpy` (XSI), `memchr`, `memcmp`, `memcpy`, `memmem`, `memmove`, `memset`, `stpcpy`, `stpncpy`, `strcat`, `strchr`, `strcmp`, `strcoll`, `strcoll_l`, `strcpy`, `strcspn`, `strdup`, `strerror`, `strerror_l`, `strerror_r`, `strlcat`, `strlcpy`, `strlen`, `strncat`, `strncmp`, `strncpy`, `strndup`, `strnlen`, `strpbrk`, `strrchr`, `strsignal`, `strspn`, `strstr`, `strtok`, `strtok_r`, `strxfrm`, `strxfrm_l` |
| `<strings.h>` | present (7) | `ffs` (XSI), `ffsl` (XSI), `ffsll` (XSI), `strcasecmp`, `strcasecmp_l`, `strncasecmp`, `strncasecmp_l` |

### Wide and multibyte characters

| Header | Status | Interfaces |
|---|---|---|
| `<inttypes.h>` | present (2) | `wcstoimax`, `wcstoumax` |
| `<uchar.h>` | present (4) | `c16rtomb`, `c32rtomb`, `mbrtoc16`, `mbrtoc32` |
| `<wchar.h>` | present (76) | `btowc`, `fgetwc`, `fgetws`, `fputwc`, `fputws`, `fwide`, `fwprintf`, `fwscanf`, `getwc`, `getwchar`, `mbrlen`, `mbrtowc`, `mbsinit`, `mbsnrtowcs`, `mbsrtowcs`, `open_wmemstream`, `putwc`, `putwchar`, `swprintf`, `swscanf`, `ungetwc`, `vfwprintf`, `vfwscanf`, `vswprintf`, `vswscanf`, `vwprintf`, `vwscanf`, `wcpcpy`, `wcpncpy`, `wcrtomb`, `wcscasecmp`, `wcscasecmp_l`, `wcscat`, `wcschr`, `wcscmp`, `wcscoll`, `wcscoll_l`, `wcscpy`, `wcscspn`, `wcsdup`, `wcsftime`, `wcslcat`, `wcslcpy`, `wcslen`, `wcsncasecmp`, `wcsncasecmp_l`, `wcsncat`, `wcsncmp`, `wcsncpy`, `wcsnlen`, `wcsnrtombs`, `wcspbrk`, `wcsrchr`, `wcsrtombs`, `wcsspn`, `wcsstr`, `wcstod`, `wcstof`, `wcstok`, `wcstol`, `wcstold`, `wcstoll`, `wcstoul`, `wcstoull`, `wcswidth` (XSI), `wcsxfrm`, `wcsxfrm_l`, `wctob`, `wcwidth` (XSI), `wmemchr`, `wmemcmp`, `wmemcpy`, `wmemmove`, `wmemset`, `wprintf`, `wscanf` |
| `<wctype.h>` | present (36) | `iswalnum`, `iswalnum_l`, `iswalpha`, `iswalpha_l`, `iswblank`, `iswblank_l`, `iswcntrl`, `iswcntrl_l`, `iswctype`, `iswctype_l`, `iswdigit`, `iswdigit_l`, `iswgraph`, `iswgraph_l`, `iswlower`, `iswlower_l`, `iswprint`, `iswprint_l`, `iswpunct`, `iswpunct_l`, `iswspace`, `iswspace_l`, `iswupper`, `iswupper_l`, `iswxdigit`, `iswxdigit_l`, `towctrans`, `towctrans_l`, `towlower`, `towlower_l`, `towupper`, `towupper_l`, `wctrans`, `wctrans_l`, `wctype`, `wctype_l` |

### Standard I/O

| Header | Status | Interfaces |
|---|---|---|
| `<stdio.h>` | present (70) | `asprintf`, `clearerr`, `dprintf`, `fclose`, `fdopen`, `feof`, `ferror`, `fflush`, `fgetc`, `fgetpos`, `fgets`, `fileno`, `flockfile`, `fmemopen`, `fopen`, `fprintf`, `fputc`, `fputs`, `fread`, `freopen`, `fscanf`, `fseek`, `fseeko`, `fsetpos`, `ftell`, `ftello`, `ftrylockfile`, `funlockfile`, `fwrite`, `getc`, `getc_unlocked`, `getchar`, `getchar_unlocked`, `getdelim`, `getline`, `open_memstream`, `pclose`, `perror`, `popen`, `printf`, `putc`, `putc_unlocked`, `putchar`, `putchar_unlocked`, `puts`, `remove`, `rename`, `renameat`, `rewind`, `scanf`, `setbuf`, `setvbuf`, `snprintf`, `sprintf`, `sscanf`, `stderr`, `stdin`, `stdout`, `tmpfile`, `tmpnam` (OB), `ungetc`, `vasprintf`, `vdprintf`, `vfprintf`, `vfscanf`, `vprintf`, `vscanf`, `vsnprintf`, `vsprintf`, `vsscanf` |

### Math and the floating-point environment

| Header | Status | Interfaces |
|---|---|---|
| `<fenv.h>` | present (11) | `feclearexcept`, `fegetenv`, `fegetexceptflag`, `fegetround`, `feholdexcept`, `feraiseexcept`, `fesetenv`, `fesetexceptflag`, `fesetround`, `fetestexcept`, `feupdateenv` |
| `<math.h>` | present (162) | `acos`, `acosf`, `acosh`, `acoshf`, `asin`, `asinf`, `asinh`, `asinhf`, `atan`, `atan2`, `atan2f`, `atanf`, `atanh`, `atanhf`, `cbrt`, `cbrtf`, `ceil`, `ceilf`, `ceill`, `copysign`, `copysignf`, `copysignl`, `cos`, `cosf`, `cosh`, `coshf`, `erf`, `erfc`, `erfcf`, `erff`, `exp`, `exp2`, `exp2f`, `expf`, `expm1`, `expm1f`, `fabs`, `fabsf`, `fabsl`, `fdim`, `fdimf`, `fdiml`, `floor`, `floorf`, `floorl`, `fma`, `fmaf`, `fmax`, `fmaxf`, `fmaxl`, `fmin`, `fminf`, `fminl`, `fmod`, `fmodf`, `fmodl`, `fpclassify`, `frexp`, `frexpf`, `frexpl`, `hypot`, `hypotf`, `ilogb`, `ilogbf`, `ilogbl`, `isfinite`, `isgreater`, `isgreaterequal`, `isinf`, `isless`, `islessequal`, `islessgreater`, `isnan`, `isnormal`, `isunordered`, `j0`, `j1`, `jn`, `ldexp`, `ldexpf`, `ldexpl`, `lgamma`, `lgammaf`, `llrint`, `llrintf`, `llrintl`, `llround`, `llroundf`, `llroundl`, `log`, `log10`, `log10f`, `log1p`, `log1pf`, `log2`, `log2f`, `logb`, `logbf`, `logbl`, `logf`, `lrint`, `lrintf`, `lrintl`, `lround`, `lroundf`, `lroundl`, `modf`, `modff`, `modfl`, `nan`, `nanf`, `nanl`, `nearbyint`, `nearbyintf`, `nearbyintl`, `nextafter`, `nextafterf`, `nextafterl`, `nexttoward`, `nexttowardf`, `nexttowardl`, `pow`, `powf`, `remainder`, `remainderf`, `remainderl`, `remquo`, `remquof`, `remquol`, `rint`, `rintf`, `rintl`, `round`, `roundf`, `roundl`, `scalbln`, `scalblnf`, `scalblnl`, `scalbn`, `scalbnf`, `scalbnl`, `signbit`, `signgam`, `sin`, `sinf`, `sinh`, `sinhf`, `sqrt`, `sqrtf`, `sqrtl`, `tan`, `tanf`, `tanh`, `tanhf`, `tgamma`, `tgammaf`, `trunc`, `truncf`, `truncl`, `y0`, `y1`, `yn` |
| `<math.h>` | absent (26) | `acoshl`, `acosl`, `asinhl`, `asinl`, `atan2l`, `atanhl`, `atanl`, `cbrtl`, `coshl`, `cosl`, `erfcl`, `erfl`, `exp2l`, `expl`, `expm1l`, `fmal`, `hypotl`, `lgammal`, `log10l`, `log1pl`, `log2l`, `sinhl`, `sinl`, `tanhl`, `tanl`, `tgammal` |
| `<math.h>` | broken (2) | `logl`, `powl`, computed in `double` |

### Complex arithmetic

| Header | Status | Interfaces |
|---|---|---|
| `<complex.h>` | present (47) | `CMPLX`, `CMPLXF`, `CMPLXL`, `cabs`, `cabsf`, `cacos`, `cacosf`, `cacosh`, `cacoshf`, `carg`, `cargf`, `casin`, `casinf`, `casinh`, `casinhf`, `catan`, `catanf`, `catanh`, `catanhf`, `ccos`, `ccosf`, `ccosh`, `ccoshf`, `cexp`, `cexpf`, `cimag`, `cimagf`, `clog`, `clogf`, `conj`, `conjf`, `cpow`, `cpowf`, `cproj`, `cprojf`, `creal`, `crealf`, `csin`, `csinf`, `csinh`, `csinhf`, `csqrt`, `csqrtf`, `ctan`, `ctanf`, `ctanh`, `ctanhf` |
| `<complex.h>` | macro only (2) | `cimagl`, `creall` |
| `<complex.h>` | absent (20) | `cabsl`, `cacoshl`, `cacosl`, `cargl`, `casinhl`, `casinl`, `catanhl`, `catanl`, `ccoshl`, `ccosl`, `cexpl`, `clogl`, `conjl`, `cpowl`, `cprojl`, `csinhl`, `csinl`, `csqrtl`, `ctanhl`, `ctanl` |

### Locales, messages and conversion

| Header | Status | Interfaces |
|---|---|---|
| `<iconv.h>` | present (3) | `iconv`, `iconv_close`, `iconv_open` |
| `<langinfo.h>` | present (2) | `nl_langinfo`, `nl_langinfo_l` |
| `<libintl.h>` | present (15) | `bind_textdomain_codeset`, `bindtextdomain`, `dcgettext`, `dcgettext_l`, `dcngettext`, `dcngettext_l`, `dgettext`, `dgettext_l`, `dngettext`, `dngettext_l`, `gettext`, `gettext_l`, `ngettext`, `ngettext_l`, `textdomain` |
| `<locale.h>` | present (6) | `duplocale`, `freelocale`, `localeconv`, `newlocale`, `setlocale`, `uselocale` |
| `<locale.h>` | absent (1) | `getlocalename_l` |
| `<monetary.h>` | absent (2) | `strfmon`, `strfmon_l` |
| `<nl_types.h>` | present (3) | `catclose`, `catgets`, `catopen` |

### Files, directories and I/O multiplexing

| Header | Status | Interfaces |
|---|---|---|
| `<dirent.h>` | present (11) | `alphasort`, `closedir`, `dirfd`, `fdopendir`, `opendir`, `readdir`, `readdir_r` (OB), `rewinddir`, `scandir`, `seekdir` (XSI), `telldir` (XSI) |
| `<dirent.h>` | absent (1) | `posix_getdents` |
| `<fcntl.h>` | present (6) | `creat`, `fcntl`, `open`, `openat`, `posix_fadvise` (ADV), `posix_fallocate` (ADV) |
| `<poll.h>` | present (2) | `poll`, `ppoll` |
| `<sys/select.h>` | present (6) | `FD_CLR`, `FD_ISSET`, `FD_SET`, `FD_ZERO`, `pselect`, `select` |
| `<sys/stat.h>` | present (16) | `chmod`, `fchmod`, `fchmodat`, `fstat`, `fstatat`, `futimens`, `lstat`, `mkdir`, `mkdirat`, `mkfifo`, `mkfifoat`, `mknod` (XSI), `mknodat` (XSI), `stat`, `umask`, `utimensat` |
| `<sys/statvfs.h>` | present (2) | `fstatvfs`, `statvfs` |
| `<sys/uio.h>` | present (2) | `readv` (XSI), `writev` (XSI) |

### Processes, identity and the system

| Header | Status | Interfaces |
|---|---|---|
| `<fmtmsg.h>` | absent (1) | `fmtmsg` (XSI) |
| `<sys/resource.h>` | present (5) | `getpriority` (XSI), `getrlimit`, `getrusage` (XSI), `setpriority` (XSI), `setrlimit` |
| `<sys/times.h>` | present (1) | `times` |
| `<sys/utsname.h>` | present (1) | `uname` |
| `<sys/wait.h>` | present (3) | `wait`, `waitid`, `waitpid` |
| `<syslog.h>` | present (4) | `closelog` (XSI), `openlog` (XSI), `setlogmask` (XSI), `syslog` (XSI) |
| `<unistd.h>` | present (85) | `_exit`, `access`, `alarm`, `chdir`, `chown`, `close`, `crypt` (XSI), `dup`, `dup2`, `dup3`, `encrypt` (XSI), `environ`, `execl`, `execle`, `execlp`, `execv`, `execve`, `execvp`, `faccessat`, `fchdir`, `fchown`, `fchownat`, `fdatasync`, `fork`, `fpathconf`, `fsync`, `ftruncate`, `getcwd`, `getegid`, `getentropy`, `geteuid`, `getgid`, `getgroups`, `gethostid` (XSI), `gethostname`, `getlogin`, `getlogin_r`, `getopt`, `getpgid`, `getpgrp`, `getpid`, `getppid`, `getresgid` (XSI), `getresuid` (XSI), `getsid`, `getuid`, `lchown`, `link`, `linkat`, `lockf` (XSI), `lseek`, `optarg`, `opterr`, `optind`, `optopt`, `pathconf`, `pause`, `pipe`, `pipe2`, `pread`, `pwrite`, `read`, `readlink`, `readlinkat`, `rmdir`, `setegid`, `seteuid`, `setgid`, `setpgid`, `setregid` (XSI), `setresgid` (XSI), `setresuid` (XSI), `setreuid` (XSI), `setsid`, `setuid`, `sleep`, `swab` (XSI), `symlink`, `symlinkat`, `sync` (XSI), `sysconf`, `truncate`, `unlink`, `unlinkat`, `write` |
| `<unistd.h>` | absent (3) | `confstr`, `nice` (XSI), `posix_close` |

### Spawning

| Header | Status | Interfaces |
|---|---|---|
| `<sched.h>` | present (4) | `posix_spawnattr_getschedparam`, `posix_spawnattr_getschedpolicy`, `posix_spawnattr_setschedparam`, `posix_spawnattr_setschedpolicy` |
| `<spawn.h>` | present (17) | `posix_spawn` (SPN), `posix_spawn_file_actions_addclose` (SPN), `posix_spawn_file_actions_adddup2` (SPN), `posix_spawn_file_actions_addopen` (SPN), `posix_spawn_file_actions_destroy` (SPN), `posix_spawn_file_actions_init` (SPN), `posix_spawnattr_destroy` (SPN), `posix_spawnattr_getflags` (SPN), `posix_spawnattr_getpgroup` (SPN), `posix_spawnattr_getsigdefault` (SPN), `posix_spawnattr_getsigmask` (SPN), `posix_spawnattr_init` (SPN), `posix_spawnattr_setflags` (SPN), `posix_spawnattr_setpgroup` (SPN), `posix_spawnattr_setsigdefault` (SPN), `posix_spawnattr_setsigmask` (SPN), `posix_spawnp` (SPN) |
| `<spawn.h>` | absent (2) | `posix_spawn_file_actions_addchdir` (SPN), `posix_spawn_file_actions_addfchdir` (SPN) |
| `<unistd.h>` | absent (2) | `_Fork`, `fexecve` |

### Signals and non-local jumps

| Header | Status | Interfaces |
|---|---|---|
| `<setjmp.h>` | present (4) | `longjmp`, `setjmp`, `siglongjmp`, `sigsetjmp` |
| `<signal.h>` | present (20) | `kill`, `killpg` (XSI), `pthread_kill`, `pthread_sigmask`, `raise`, `sigaction`, `sigaddset`, `sigaltstack` (XSI), `sigdelset`, `sigemptyset`, `sigfillset`, `sigismember`, `signal`, `sigpending`, `sigprocmask`, `sigqueue`, `sigsuspend`, `sigtimedwait`, `sigwait`, `sigwaitinfo` |
| `<signal.h>` | absent (4) | `psiginfo`, `psignal`, `sig2str`, `str2sig` |

### Time and clocks

| Header | Status | Interfaces |
|---|---|---|
| `<sys/time.h>` | present (1) | `utimes` (XSI) |
| `<time.h>` | present (25) | `asctime` (OB), `asctime_r`, `clock`, `clock_getres`, `clock_gettime`, `clock_nanosleep`, `clock_settime`, `ctime` (OB), `ctime_r`, `daylight` (XSI), `difftime`, `gmtime`, `gmtime_r`, `localtime`, `localtime_r`, `mktime`, `nanosleep`, `pthread_getcpuclockid` (TCT), `strftime`, `strftime_l`, `strptime` (XSI), `time`, `timezone` (XSI), `tzname`, `tzset` |
| `<time.h>` | absent (3) | `getdate` (XSI), `getdate_err`, `timespec_get` |

### Threads and scheduling

| Header | Status | Interfaces |
|---|---|---|
| `<pthread.h>` | present (97) | `pthread_atfork` (OB), `pthread_attr_destroy`, `pthread_attr_getdetachstate`, `pthread_attr_getguardsize`, `pthread_attr_getinheritsched` (TPS), `pthread_attr_getschedparam`, `pthread_attr_getschedpolicy` (TPS), `pthread_attr_getscope` (TPS), `pthread_attr_getstack`, `pthread_attr_getstacksize` (TSS), `pthread_attr_init`, `pthread_attr_setdetachstate`, `pthread_attr_setguardsize`, `pthread_attr_setinheritsched` (TPS), `pthread_attr_setschedparam`, `pthread_attr_setschedpolicy` (TPS), `pthread_attr_setscope` (TPS), `pthread_attr_setstack`, `pthread_attr_setstacksize` (TSS), `pthread_barrier_destroy`, `pthread_barrier_init`, `pthread_barrier_wait`, `pthread_barrierattr_destroy`, `pthread_barrierattr_getpshared` (TSH), `pthread_barrierattr_init`, `pthread_barrierattr_setpshared` (TSH), `pthread_cancel`, `pthread_cleanup_pop`, `pthread_cleanup_push`, `pthread_cond_broadcast`, `pthread_cond_destroy`, `pthread_cond_init`, `pthread_cond_signal`, `pthread_cond_timedwait`, `pthread_cond_wait`, `pthread_condattr_destroy`, `pthread_condattr_getclock`, `pthread_condattr_getpshared` (TSH), `pthread_condattr_init`, `pthread_condattr_setclock`, `pthread_condattr_setpshared` (TSH), `pthread_create`, `pthread_detach`, `pthread_equal`, `pthread_exit`, `pthread_getschedparam` (TPS), `pthread_getspecific`, `pthread_join`, `pthread_key_create`, `pthread_key_delete`, `pthread_mutex_consistent`, `pthread_mutex_destroy`, `pthread_mutex_getprioceiling`, `pthread_mutex_init`, `pthread_mutex_lock`, `pthread_mutex_setprioceiling`, `pthread_mutex_timedlock`, `pthread_mutex_trylock`, `pthread_mutex_unlock`, `pthread_mutexattr_destroy`, `pthread_mutexattr_getprioceiling`, `pthread_mutexattr_getprotocol` (MC1), `pthread_mutexattr_getpshared` (TSH), `pthread_mutexattr_getrobust`, `pthread_mutexattr_gettype`, `pthread_mutexattr_init`, `pthread_mutexattr_setprioceiling`, `pthread_mutexattr_setprotocol` (MC1), `pthread_mutexattr_setpshared` (TSH), `pthread_mutexattr_setrobust`, `pthread_mutexattr_settype`, `pthread_once`, `pthread_rwlock_destroy`, `pthread_rwlock_init`, `pthread_rwlock_rdlock`, `pthread_rwlock_timedrdlock`, `pthread_rwlock_timedwrlock`, `pthread_rwlock_tryrdlock`, `pthread_rwlock_trywrlock`, `pthread_rwlock_unlock`, `pthread_rwlock_wrlock`, `pthread_rwlockattr_destroy`, `pthread_rwlockattr_getpshared` (TSH), `pthread_rwlockattr_init`, `pthread_rwlockattr_setpshared` (TSH), `pthread_self`, `pthread_setcancelstate`, `pthread_setcanceltype`, `pthread_setschedparam` (TPS), `pthread_setschedprio` (TPS), `pthread_setspecific`, `pthread_spin_destroy`, `pthread_spin_init`, `pthread_spin_lock`, `pthread_spin_trylock`, `pthread_spin_unlock`, `pthread_testcancel` |
| `<pthread.h>` | absent (4) | `pthread_cond_clockwait`, `pthread_mutex_clocklock`, `pthread_rwlock_clockrdlock`, `pthread_rwlock_clockwrlock` |
| `<sched.h>` | present (8) | `sched_get_priority_max` (PS or TPS), `sched_get_priority_min` (PS or TPS), `sched_getparam` (PS), `sched_getscheduler` (PS), `sched_rr_get_interval` (PS or TPS), `sched_setparam` (PS), `sched_setscheduler` (PS), `sched_yield` |
| `<semaphore.h>` | present (11) | `sem_clockwait`, `sem_close`, `sem_destroy`, `sem_getvalue`, `sem_init`, `sem_open`, `sem_post`, `sem_timedwait`, `sem_trywait`, `sem_unlink`, `sem_wait` |
| `<threads.h>` | present (25) | `call_once`, `cnd_broadcast`, `cnd_destroy`, `cnd_init`, `cnd_signal`, `cnd_timedwait`, `cnd_wait`, `mtx_destroy`, `mtx_init`, `mtx_lock`, `mtx_timedlock`, `mtx_trylock`, `mtx_unlock`, `thrd_create`, `thrd_current`, `thrd_detach`, `thrd_equal`, `thrd_exit`, `thrd_join`, `thrd_sleep`, `thrd_yield`, `tss_create`, `tss_delete`, `tss_get`, `tss_set` |

### Memory mapping and System V IPC

| Header | Status | Interfaces |
|---|---|---|
| `<sys/ipc.h>` | present (1) | `ftok` (XSI) |
| `<sys/mman.h>` | present (11) | `mlock` (MLR), `mlockall` (ML), `mmap`, `mprotect`, `msync` (XSI or SIO), `munlock` (MLR), `munlockall` (ML), `munmap`, `posix_madvise` (ADV), `shm_open` (SHM), `shm_unlink` (SHM) |
| `<sys/msg.h>` | present (4) | `msgctl` (XSI), `msgget` (XSI), `msgrcv` (XSI), `msgsnd` (XSI) |
| `<sys/sem.h>` | present (3) | `semctl` (XSI), `semget` (XSI), `semop` (XSI) |
| `<sys/shm.h>` | present (4) | `shmat` (XSI), `shmctl` (XSI), `shmdt` (XSI), `shmget` (XSI) |

### Realtime: asynchronous I/O, message queues, timers, shared memory

| Header | Status | Interfaces |
|---|---|---|
| `<aio.h>` | absent (8) | `aio_cancel`, `aio_error`, `aio_fsync` (FSC or SIO), `aio_read`, `aio_return`, `aio_suspend`, `aio_write`, `lio_listio` |
| `<mqueue.h>` | absent (8) | `mq_close` (MSG), `mq_notify` (MSG), `mq_open` (MSG), `mq_receive` (MSG), `mq_send` (MSG), `mq_timedreceive` (MSG), `mq_timedsend` (MSG), `mq_unlink` (MSG) |
| `<mqueue.h>` | present (2) | `mq_getattr` (MSG), `mq_setattr` (MSG) |
| `<sys/mman.h>` | absent (3) | `posix_mem_offset` (TYM), `posix_typed_mem_get_info` (TYM), `posix_typed_mem_open` (TYM) |
| `<time.h>` | absent (1) | `clock_getcpuclockid` (CPT) |
| `<time.h>` | present (5) | `timer_create`, `timer_delete`, `timer_getoverrun`, `timer_gettime`, `timer_settime` |

### Terminals and devices

| Header | Status | Interfaces |
|---|---|---|
| `<devctl.h>` | absent (1) | `posix_devctl` (DC) |
| `<stdio.h>` | absent (1) | `ctermid` |
| `<stdlib.h>` | present (5) | `grantpt` (XSI), `posix_openpt` (XSI), `ptsname` (XSI), `ptsname_r` (XSI), `unlockpt` (XSI) |
| `<termios.h>` | present (13) | `cfgetispeed`, `cfgetospeed`, `cfsetispeed`, `cfsetospeed`, `tcdrain`, `tcflow`, `tcflush`, `tcgetattr`, `tcgetsid`, `tcgetwinsize`, `tcsendbreak`, `tcsetattr`, `tcsetwinsize` |
| `<unistd.h>` | present (5) | `isatty`, `tcgetpgrp`, `tcsetpgrp`, `ttyname`, `ttyname_r` |

### Networking and name resolution

| Header | Status | Interfaces |
|---|---|---|
| `<arpa/inet.h>` | present (8) | `htonl`, `htons`, `inet_addr` (OB), `inet_ntoa` (OB), `inet_ntop`, `inet_pton`, `ntohl`, `ntohs` |
| `<net/if.h>` | present (4) | `if_freenameindex`, `if_indextoname`, `if_nameindex`, `if_nametoindex` |
| `<netdb.h>` | present (22) | `endhostent`, `endnetent`, `endprotoent`, `endservent`, `freeaddrinfo`, `gai_strerror`, `getaddrinfo`, `gethostent`, `getnameinfo`, `getnetbyaddr`, `getnetbyname`, `getnetent`, `getprotobyname`, `getprotobynumber`, `getprotoent`, `getservbyname`, `getservbyport`, `getservent`, `sethostent`, `setnetent`, `setprotoent`, `setservent` |
| `<netinet/in.h>` | present (2) | `in6addr_any` (IP6), `in6addr_loopback` (IP6) |
| `<sys/socket.h>` | present (19) | `accept`, `accept4`, `bind`, `connect`, `getpeername`, `getsockname`, `getsockopt`, `listen`, `recv`, `recvfrom`, `recvmsg`, `send`, `sendmsg`, `sendto`, `setsockopt`, `shutdown`, `sockatmark`, `socket`, `socketpair` |

### Patterns, paths and search

| Header | Status | Interfaces |
|---|---|---|
| `<fnmatch.h>` | present (1) | `fnmatch` |
| `<ftw.h>` | present (1) | `nftw` (XSI) |
| `<glob.h>` | present (2) | `glob`, `globfree` |
| `<libgen.h>` | present (2) | `basename` (XSI), `dirname` (XSI) |
| `<regex.h>` | present (4) | `regcomp`, `regerror`, `regexec`, `regfree` |
| `<search.h>` | present (11) | `hcreate` (XSI), `hdestroy` (XSI), `hsearch` (XSI), `insque` (XSI), `lfind` (XSI), `lsearch` (XSI), `remque` (XSI), `tdelete` (XSI), `tfind` (XSI), `tsearch` (XSI), `twalk` (XSI) |
| `<wordexp.h>` | absent (2) | `wordexp`, `wordfree` |

### Users, groups and databases

| Header | Status | Interfaces |
|---|---|---|
| `<grp.h>` | present (7) | `endgrent` (XSI), `getgrent` (XSI), `getgrgid`, `getgrgid_r`, `getgrnam`, `getgrnam_r`, `setgrent` (XSI) |
| `<ndbm.h>` | absent (9) | `dbm_clearerr` (XSI), `dbm_close` (XSI), `dbm_delete` (XSI), `dbm_error` (XSI), `dbm_fetch` (XSI), `dbm_firstkey` (XSI), `dbm_nextkey` (XSI), `dbm_open` (XSI), `dbm_store` (XSI) |
| `<pwd.h>` | present (7) | `endpwent` (XSI), `getpwent` (XSI), `getpwnam`, `getpwnam_r`, `getpwuid`, `getpwuid_r`, `setpwent` (XSI) |
| `<utmpx.h>` | present (6) | `endutxent` (XSI), `getutxent` (XSI), `getutxid` (XSI), `getutxline` (XSI), `pututxline` (XSI), `setutxent` (XSI) |

### Dynamic loading

| Header | Status | Interfaces |
|---|---|---|
| `<dlfcn.h>` | present (5) | `dladdr`, `dlclose`, `dlerror`, `dlopen`, `dlsym` |
