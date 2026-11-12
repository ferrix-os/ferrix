# Dynamic linking — PIE, `PT_INTERP`, a loader  ·  *done 2026-09-23: 39 points, and ferrousli's port at ≈ 34*

Placed after *Networking* without a number of its own, for the same reason:
nothing on the path to `rustc` needs it, since Rust's `std` targets static
musl. What needs it is the promise `docs/ARCHITECTURE.md` §2 makes — that
somebody else's Linux binary runs unchanged — which today holds only for a
static, fixed-address executable. Nearly every binary a distribution ships is a position-independent executable
that asks for glibc's `ld-linux`. `src/kernel/extensions/personality/linux/src/syscall/load.rs` used to refuse
`PT_INTERP` by name; since 2026-09-20 it loads the linker the program asks for
and enters it, which is the first bullet below. Since the same day there is a
linker to name — `src/user/system/linux/ferrousli/ld`, the second bullet — and since 2026-09-21 it
runs Debian's glibc busybox inside Ferrix on x86-64, which is the exit's
second half there.
The question of 2026-09-16 that put this here was
whether Steam could run; the answer began with this section, before the
32-bit ABI, networking and the display stages it also waits on. The 32-bit
x86 ABI is not part of it; since 2026-09-18 it is stage 22's, where the rest
of that answer is.

Three parts, in the order they can be tested:

* **The kernel half, 5 points — done, 2026-09-21.** `execve` loads
  an `ET_DYN` executable at a
  base of its own — Linux's `ELF_ET_DYN_BASE`, unrandomised until stage 13 —
  and applies its relative relocations, which `src/lib/platform/elf` already reads
  because the UEFI loader relocates itself. A `PT_INTERP` names a second
  file: the interpreter is loaded at its own base, the entry point is the
  interpreter's, and the auxiliary vector says the rest — `AT_BASE` for the
  interpreter, `AT_PHDR`, `AT_PHNUM` and `AT_ENTRY` for the program, plus
  `AT_RANDOM`, `AT_EXECFN` and `AT_PLATFORM`, whose keys `src/lib/proto/linux-abi`
  carries. The interpreter then maps libraries itself, through stage 8's
  file-backed `mmap` with `MAP_FIXED` and `PROT_EXEC`, and `mprotect`s its
  `PT_GNU_RELRO` — now covered in the same pattern a real `ld.so` uses.
  `AT_SYSINFO_EHDR` stays absent: there is no vDSO, and glibc
  and musl both fall back to the real call. (Since 2026-09-26 x86-64 has
  one, and passes it: `docs/CHROME.md` §3.)

  **What landed on 2026-09-20.** Both images are placed — the program at
  `PIE_BASE`, the linker at a new `INTERP_BASE` a third of the way up the user
  half, below the program because the heap grows from where the program ends —
  and the processor is entered at the linker's entry. `AT_BASE` is filled and
  was not there at all before; `AT_PHDR`, `AT_PHNUM` and `AT_ENTRY` stay the
  program's, which is why `Loaded` now carries `start` beside `entry`. The
  linker's path comes from `Elf::interpreter`, new in `src/lib/platform/elf` with five
  tests, and is read before `execve`'s point of no return so a missing linker
  leaves the caller running. One latent bug went with it: the old refusal
  looked for `PT_INTERP` only on an `ET_DYN`, so a dynamically linked `ET_EXEC`
  was loaded and entered with no linker and every imported symbol an
  unrelocated zero.

  `AT_PLATFORM` landed 2026-09-20 too: `arch::user_platform` gives `"x86_64"`
  on x86-64 and `"v7l"` on ARMv7-A, as the Linux kernels for those
  architectures do, and `None` on AArch64, which defines no `ELF_PLATFORM`
  string at all.

  **The fifth point, 2026-09-21.** Its
  file-mapping self-check now maps a loader-shaped fixed file-backed RX text
  page and RW data page, writes the latter as its `PT_GNU_RELRO` span, and
  `mprotect`s it read-only before proving a later write is refused. That is
  the exact `MAP_FIXED`, `PROT_EXEC` and RELRO pattern a real `ld.so` uses,
  rather than three calls that only happen to exist separately.
* **ferrousli's loader, 21 points — 18 done: a first version on 2026-09-20,
  the rest by 2026-09-22.** The
  fifth item of `src/user/system/linux/ferrousli/README.md`: a dynamic loader in Rust, shipped as
  ferrousli's `ld.so` with `libferrousli.so` beside `libferrousli.a`.
  `src/user/system/linux/ferrousli/ld` reads `PT_DYNAMIC`, resolves `DT_NEEDED` libraries (through
  `LD_LIBRARY_PATH` when a name carries no path of its own), looks symbols up
  through the GNU hash table, and applies `GLOB_DAT`, `JUMP_SLOT` and
  `IRELATIVE` relocations, then runs `DT_INIT_ARRAY` in dependency order and
  enters the program. `tests/link.rs` proves it end to end against a fixture
  built by the host's own `cc`, checking a data symbol, a function pointer and
  a pointer into a library's own data all resolved. `DT_RUNPATH` is now
  covered too: a program finds a `DT_NEEDED` library in a private directory
  with `LD_LIBRARY_PATH` absent. So are `DT_FINI_ARRAY` and `DT_FINI`: the
  loader preserves its completed scope, passes its `rtld_fini` callback
  through `rdx`, and Ferrousli's runtime runs a dependency's finalisers after
  `main` returns, array entries in reverse order before the legacy function.
  x86-64 initial-exec TLS is live too: the loader lays out every `PT_TLS`
  image below `%fs`, copies both program and dependency images before their
  constructors, and applies `R_X86_64_TPOFF64`; the host fixture proves each
  image's initial value and a dependency's persistent block.

  **12 of the 21 done, 2026-09-21: what a glibc program needs of it, on
  x86-64, and run inside Ferrix.** Symbol versions: a reference naming
  `printf@GLIBC_2.2.5` is answered only by a definition carrying that
  version (or none), a hidden `name@VERSION` only by a reference naming it,
  and the hash chain is walked past definitions that do not answer.
  `COPY` relocations, which a glibc x86-64 program uses for `stdout`,
  `optind`, `__environ` and the rest, found in the libraries and never in
  the program's own room for them. The loader is in its own scope, last, so
  a `DT_NEEDED` naming the interpreter (glibc's AArch64 and ARM programs name
  `ld-linux` as a library too) is recognised rather than loaded twice, and so
  that it can export `__ferrousli_loader`: the static TLS layout, a call that
  fills a new thread's blocks, and a call that runs the program's own
  initialisers. The loader no longer runs those itself — glibc's division:
  the C library, started first, asks — which also ended a double run of a
  program's constructors under a statically linked ferrousli. And a bug:
  the page of `.bss` just past the file was left `PROT_NONE` whenever the
  segment's offset in its page pushed the end over a boundary; a fixture at
  eight sizes now proves every page writable, and failed on the old code.
  `cargo xtask check --ferrousli` ran none of `ld/`'s tests before this —
  `cargo test` at a workspace root that is also a package tests that package
  alone — so it now passes `--workspace`.

  **The general-dynamic TLS forms, 3 more, 2026-09-22, on x86-64:**
  `DTPMOD`, `DTPOFF`, the symbol-less local-dynamic forms and `TLSDESC`, and
  the loader exports `__tls_get_addr`. Every module is loaded before the
  program starts, so every block is in static TLS and both calls answer from
  a table of module offsets, with no dynamic thread vector; `dlopen` will
  need one. `tests/link.rs` builds a library in both of x86-64's dialects
  and requires its general-dynamic answer to be the address the program's
  initial-exec access finds; zeroing the table fails the `gnu` build and
  skewing a descriptor fails `gnu2`.

  **`dlfcn.h`, 3 more, 2026-09-22:** `dlopen`, `dlsym`, `dlclose`,
  `dlerror`, `dladdr` and `dl_iterate_phdr` in the loader (`ld/src/dl.rs`),
  which `libc.so.6`'s functions of those names forward to (interface
  revision 2) and a static program still answers alone. A handle is the
  object's slot in the loader's fixed scope array, checked on every use; one
  lock guards the scope and is never held across a constructor, a callback
  or an ifunc resolver, so any of them may call back in. The scope is saved
  before start-up's constructors now, which may already `dlsym`, and
  `dl_fini` finishes objects in the reverse of the order they were
  initialised, a `dlopen`ed library first. `dl_iterate_phdr` lists every
  object with its headers, which an unwinder needs for an exception thrown
  in a library. Not glibc's: nothing is unloaded (as musl), every object is
  global (`RTLD_LOCAL` is `RTLD_GLOBAL`), `RTLD_NEXT` is refused, and a
  library with its own `PT_TLS` cannot be opened yet -- its block would need
  a dynamic thread vector -- and is refused with a message and forgotten.
  (Both since answered, for Chrome, 2026-09-26: `RTLD_NEXT` through the
  caller's address, and a `dlopen`ed library's TLS in a static surplus;
  `docs/CHROME.md` §8.)
  `tests/link.rs` calls all six from a program with no C library, through a
  stub whose `SONAME` is the loader's own name; refusing no TLS library
  fails it with 98 and a `dlsym` that finds nothing with 92. A glibc-built
  program on example, pointed at ferrousli's `ld.so` and `libc.so.6`,
  `dlopen`s a library, calls into it, and gets `dladdr`'s and `dlerror`'s
  answers through `libc.so.6`.

  **AArch64 and ARMv7-A, the last 3, 2026-09-23.** Each has its `_start`,
  its hand-over to the program with `dl_fini` in `x0` or `r0`, and its
  thread pointer, `tpidr_el0` or the kernel's `set_tls`. The load bias comes
  from `__ehdr_start`, the one address a PC-relative instruction gives, and
  the program headers behind it, so nothing depends on how each linker lays
  out its GOT; self-relocation reads ARMv7-A's `REL` table as well as
  `RELA`, and walks the dynamic table with an `if` chain, since a `match`
  may become a jump table nothing has relocated yet. TLS is variant I there:
  the blocks go upwards from the thread pointer, the program's first, at
  the control block's size rounded to its alignment, which is where the
  static linker put its local-exec variables; `__tls_get_addr` is assembly
  on both, and AArch64's descriptors answer from static TLS as x86-64's
  do. The Arm loaders are linked by rust-lld as plain static PIEs, needing
  no C toolchain for either; AArch64's is built without outline atomics,
  whose helpers bring a constructor that calls `getauxval`, which a loader
  cannot import. The first boot found the same constructor in `libc.so.6`,
  run by the loader before the C library has a thread: `getauxval` set
  `errno` through a thread pointer of zero. `errno` now lives in a static
  until the first control block exists, and the loader's Arm TLS mapping
  keeps a zeroed page below the thread pointer, where ferrousli's control
  block goes. Lazy binding is not in it: everything is bound at load, as
  `LD_BIND_NOW` does, so there is no resolver trampoline to write per
  architecture. ARMv7-A's TLS descriptors are not either, since GCC's ARM
  code asks `__tls_get_addr` unless told otherwise.

  The gate gap is closed too: `cargo xtask check --ferrousli` now runs
  clippy on the loader for all three targets -- its binary needs the
  `loader` feature and a musl target, so `--all-targets` never built it --
  and on the library for the two Arm targets. The loader's ~200 findings
  were mostly `pub` items a binary never exports and unsafe blocks holding
  several operations, and the one real one was a field written and never
  read.
* **glibc's names, 13 points — 10 done on x86-64, 2026-09-21.**
  `src/user/system/linux/ferrousli/tools/build-shared.sh` links `libferrousli.a` whole into a
  `libc.so.6` whose every symbol carries the version glibc gives it by
  default, from `tools/glibc-versions/x86_64.txt` (3,733 names, which
  `tools/gen-glibc-versions.py` reads out of glibc's own libraries: names and
  version strings, the interface, nothing of the code). glibc's other names
  — `libm.so.6`, `libpthread.so.0`, `libresolv.so.2` and the rest, empty
  since glibc 2.34 but for `libm` — are answered by that one object when no
  file of their own is found. The startup contract is `__libc_start_main`
  taking glibc's arguments, as it already did, now also as a shared object:
  it asks the loader to run the program's initialisers (or runs a pre-2.34
  `crt1.o`'s `init`), and takes the TLS layout from it. Debian's busybox
  asked for 28 names the library lacked, all added: the large-file `64`
  names, `__open_2`, `__strcpy_chk` and the fortified rest it calls,
  `__syslog_chk`, the extended-attribute calls, `gnu_dev_major` and its
  pair, `setresuid`, `mallopt`, `__cmsg_nxthdr`, and GNU's
  `re_compile_pattern`, `re_search` and `re_syntax_options`. `regex_t` and
  `regmatch_t` took glibc's layout, header and code together: a program
  built against glibc allocates glibc's 8-byte `regmatch_t`, and musl's 16
  would have overrun it. `environ` became `__environ` with weak `environ`
  and `_environ`, as glibc names it, because the program copies
  `__environ`; the library reaches every such variable through its GOT,
  which rustc does unasked for an exported variable, so the copy is the one
  it uses. `_dl_start_user` and `_rtld_global` turned out to be glibc's
  business between its own `ld.so` and `libc.so.6`, which no program built
  against it reaches, and are not needed.

  **AArch64 and ARMv7-A, the last 3, 2026-09-23.** `build-shared.sh --arch`
  builds either, linking `libc.so.6` with the toolchain's rust-lld, and
  `tools/glibc-versions/{aarch64,armv7a}.txt` are read from Debian 13's
  glibc 2.41, the one the exit's busybox came with. AArch64's is a copy of
  x86-64's in kind. ARMv7-A's is not: a program built there with
  `_TIME_BITS=64`, as Debian's are since its time64 transition, calls
  glibc's own time64 names, `__clock_gettime64` for `clock_gettime` and
  `__stat64_time64` for `stat`. `src/glibc_time64.rs` answers them: most
  are the library's function under another name, and nine pass glibc's
  structures, whose layouts were read from glibc's armhf headers with a
  probe and differ from musl's -- the four `stat` calls, 112 bytes against
  152; `semctl`, `shmctl` and `msgctl`, whose `*_ds` keep one 64-bit field
  per time where musl keeps the kernel's halves; and `adjtimex`, whose
  `struct timex` is the kernel's own. The names glibc keeps only for a
  32-bit `time_t` or `off_t` -- `time`, `stat`, `lseek` and 156 more -- are
  marked `-` in the table and left out of `libc.so.6`, so a program built
  without 64-bit time fails to load, naming the function, instead of
  calling one with the wrong structure. So are the time64 names whose
  structure is handed to a callback (`glob`, `ftw`, `fts`), and
  `__clock_adjtime64`, which musl uses for its own `struct timex`.

`cargo xtask test-shell` gained `--interpreter` and `--library` on
2026-09-21. The first puts a linker in the initramfs at the path `--init`'s own
`PT_INTERP` names, read from the program so the test cannot pass by putting it
somewhere the program would not look; the second puts each library in `/lib`,
which glibc's linker and ferrousli's both search with no configuration. The
built-in shell now reads the linker it names through the VFS, as a command
from the initramfs already did, so the test binary is still one the
repository does not carry. `tools/common/fetch/fetch-debian-busybox.sh` fetches the one
the exit names, pinned by checksum.

**Exit,** in two halves, each a test of its own for the reason stage 7's is:

1. A distribution's dynamic busybox — Debian's, linked against glibc — with
   its own `ld-linux-x86-64.so.2` and `libc.so.6` on the image, runs stage
   7's `test-shell` script on x86-64 and AArch64, and the `armhf` pair does
   the same on ARMv7-A, printing the same lines and exiting 7. This proves
   the kernel half against a loader nobody here wrote.

   **Met on 2026-09-21,** on all three architectures and at the first
   attempt: Debian 13's busybox 1.37.0-6+b9 with glibc 2.41-12+deb13u4's
   linker, `libc.so.6` and `libresolv.so.2`, as `tools/common/fetch/fetch-debian-busybox.sh`
   lays them out —

   ```
   D=~/.local/share/ferrix/busybox/debian
   cargo xtask test-shell --arch all --init "$D/{arch}/busybox" \
       --interpreter "$D/{arch}/ld.so" \
       --library "$D/{arch}/libc.so.6" --library "$D/{arch}/libresolv.so.2"
   ```

   The same x86-64 run without `--interpreter` stops at `the shell could not
   be started: its linker: errno 2`, which is the control that shows the
   linker was the one loaded.
2. The same binary with glibc's files removed and ferrousli's `ld.so` and
   `libferrousli.so` at their paths, running the same script on all three
   architectures. This proves the other two parts, and is the README's fifth
   item in its entirety.

   **Met on x86-64, 2026-09-21:** nothing of glibc on the image, ferrousli's
   loader at `/lib64/ld-linux-x86-64.so.2` and ferrousli as `/lib/libc.so.6`
   (`libresolv.so.2` answered by it), and the script prints its lines and
   exits 7 —

   ```
   D=~/.local/share/ferrix/busybox/debian
   cargo xtask test-shell --arch x86_64 --init "$D/{arch}/busybox" \
       --interpreter ferrousli --library ferrousli
   ```

   Without `--library` it stops at `ld-ferrousli: library not found:
   libc.so.6` and 127.

   **Met on AArch64 and ARMv7-A, 2026-09-23,** and with it the whole exit:
   the same busybox for each, with ferrousli's loader at the path its
   `PT_INTERP` names and ferrousli as `/lib/libc.so.6`, prints the
   script's lines and exits 7 --

   ```
   cargo xtask test-shell --arch aarch64 --init "$D/{arch}/busybox" \
       --interpreter ferrousli --library ferrousli
   cargo xtask test-shell --arch armv7a --init "$D/{arch}/busybox" \
       --interpreter ferrousli --library ferrousli
   ```

   and ARMv7-A's without `--library` stops at `ld-ferrousli: library not
   found: libc.so.6` and 127, as x86-64's does. Before this landing the
   check was static as well: every symbol either busybox imports is
   defined by ferrousli's `libc.so.6` at the version it asks for, but for
   three weak ones glibc does not define either.

**Done — ferrousli on AArch64 and ARMv7-A, 2026-09-23.** The customer put
the library's port inside this stage on 2026-09-21, at ≈ 34 points of its
own. ferrousli now builds for both, and its whole suite passes on each under
QEMU 9.2.4's user mode — every unit test, and every C program at `-O0` and
`-O2` — with x86-64's unchanged. `src/user/system/linux/ferrousli/README.md` says how to run it.
What it took: a system-call table per architecture, generated from the
kernel's headers; the thread pointer, `clone` and TLS variant I, with the
canary in `__stack_chk_guard`; `setjmp`, `va_list`, `fenv`, signal
restorers, cancellation's system call and `crt1.o` for each; AArch64's
`long double` as IEEE binary128 in software; and on ARMv7-A the 32-bit
layouts with a 64-bit `time_t` and `off_t`, the kernel's time64 calls, and
conversions where it has none (`itimerval`, `rusage`, SysV IPC's `*_ds`,
`stat64`, `statfs64`, `timex`, the socket timeouts). Three
synchronisation objects had assumed 64 bits and were the wrong size for
C's on ARMv7-A — `pthread_cond_t`, the read-write lock and `sem_t` — and a
timed condition wait hung on stack garbage until they were fixed.

busybox built against ferrousli runs on all three since the same evening:
`tools/busybox/build.sh --arch` cross-compiles it with gcc for AArch64 or
ARMv7-A against Alpine's pinned UAPI headers for each, it linked on both at
the first attempt, and `cargo xtask test-shell --arch aarch64|armv7a --init
ferrousli` builds it and runs stage 7's script to 7 on each. Not yet: CI
runs only x86-64's suite, because the cross compilers and QEMU's user mode
the Arm suites need are not on its runners; `docs/BACKLOG.md` has the row.

**Done — AArch64's string routines, for the Pixel 7, 2026-09-23.** The
customer means to run ferrousli on a rooted Pixel 7 that boots Ferrix
itself, whose Tensor G2 is Cortex-X1, A78 and A55 cores. On AArch64
`memcpy`, `memmove`, `memset`, `memcmp`, `memchr`, `strlen` and
`strchrnul` work sixteen bytes at a time in Advanced SIMD registers
(`src/user/system/linux/ferrousli/src/string/aarch64.rs`), which every AArch64 core has, so
nothing is chosen at run time; copies and fills of 64 bytes and more are
one loop of `ldp`/`stp` register pairs. Counted with QEMU's instruction
plugin on a Cortex-A55, at 4 KiB they take 1.8 to 5.3 times fewer
instructions than the generic code as LLVM compiles it -- which already
vectorises the byte-copying loops by itself, so a first version with
separate sixteen-byte loads was slower than it and was rewritten. The
atomics needed nothing: the outline helpers every atomic calls take LSE
instructions once a constructor reads `HWCAP_ATOMICS`, and a test holds
that to `AT_HWCAP`. The string and thread suites pass under QEMU at
`-cpu` `cortex-a55` and `cortex-a76` (the phone's cores), `cortex-a72`
(Ferrix's QEMU machine), `cortex-a53` (no LSE) and `max`. Outside the
stage's points.


**Done — `rustc`'s libraries on the loader, 2026-09-26.** Running the
rustc volume on ferrousli (`docs/roadmap/stage-16-rustc.md`) took four
changes to the loader, each with a test in `src/user/system/linux/ferrousli/ld/tests/link.rs`
that fails with it taken out: `$ORIGIN` and `${ORIGIN}` in a run path,
from `/proc/self/exe` for the program and the path found for a library, and
not for an `AT_SECURE` program; a program header table read from `e_phoff`
when it is not in the first page; constructors in dependency order,
depth first over `DT_NEEDED`, as glibc's `_dl_sort_maps` orders them, where
they had run in the reverse of load order; and a definition whose version
index names a version the object needs -- a program's copy of a library's
variable -- answering a reference to that version, as glibc numbers both
kinds in one table. The last one had kept every glibc program's `environ`
null on ferrousli. Beside them, `libc.so.6` lays out `pthread_mutex_t` as
glibc does and prepares the process from its own constructor. Outside the
stage's points.

---

