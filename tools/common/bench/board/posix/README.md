# POSIX programs on the DK1: lmbench and speedtest1 (B5)

Level 4 of `docs/BOARD-BENCH.md`, "Programs": the same static ARMv7-A
binaries run on Linux 7.2.9 and on Ferrix on the STM32MP157D-DK1, and each
figure is reported as a ratio to Linux's. Nothing here is a result until it
has run on the board; every timing printed under QEMU is TCG time, which
says nothing about hardware.

| File | What |
|---|---|
| `build.sh` | Fetches, checks and builds everything; installs the programs, the run scripts, `SHA256SUMS` and `BUILD-INFO`. |
| `pins.sh` | The URLs and checksums `build.sh` fetches against. |
| `run-lmbench.sh` | The lmbench set with fixed arguments; one tagged line per result. |
| `run-speedtest1.sh` | speedtest1 on an in-memory and on a file database; one tagged line per test. |
| `exec-check.c` | Ours: checks that `lat_proc`'s exec really runs `hello` (below). |

## Building

```
tools/common/bench/board/posix/build.sh [--work DIR] [--out DIR] [--jobs N]
```

On nazuna it takes about a minute. Defaults: work in
`~/.local/share/ferrix/board-bench/posix-work`, install into
`~/.local/share/ferrix/board-bench/posix` (`bin/`, the two run scripts,
`SHA256SUMS`, `BUILD-INFO`). It needs `arm-linux-gnueabihf-gcc` (nazuna:
Ubuntu's 15.2.0), make, curl, unzip or python3, sha256sum, openssl or
sha3sum, and gpg for musl's signature (without gpg the sha256 pin alone is
checked, and `BUILD-INFO` says so). No source is vendored.

### Sources

| What | Pin | Checked by |
|---|---|---|
| musl 1.2.5 | the release tarball | its `.asc`, verified with musl's key 8364 8929 0BB6 B70F 99FF DA05 56BC DB59 3020 450F (the same key keyserver.ubuntu.com lists for musl@libc.org), and its sha256. The copy in `~/.local/share/ferrix/ferrousli-ref/` on nazuna is the same file, and its unpacked tree matches the tarball's. |
| lmbench 3.0-a9 | the original release on SourceForge | the sha1 and md5 SourceForge publishes, and the sha256 pinned from that file |
| SQLite 3.53.4 | `sqlite-amalgamation-3530400.zip`, and `test/speedtest1.c` from `sqlite-src-3530400.zip` (check-in bf7c7f30...) | the SHA3-256 sqlite.org lists for each archive, and a pinned sha256 |

### Flags

Every C file, musl included, is compiled with
`-mcpu=cortex-a7 -mfpu=neon-vfpv4 -mfloat-abi=hard -marm -O2`, and linked
`-static -no-pie`. The instruction-set flag is one variable, `BENCH_ISA`,
because every C bench on the board must use the same one; it is `-marm`
because Ferrix's armv7a programs are A32 (B3, 2026-10-08), and it is never
left to the default, because Ubuntu's gcc defaults to Thumb-2. Ubuntu's
hardening defaults are turned off by name (`-fno-stack-protector
-fno-stack-clash-protection -U_FORTIFY_SOURCE -fno-pie`) so that another
gcc 15.2 compiles the same code.

The only Thumb code in the binaries is gcc's own: `crtbegin.o`'s
`deregister_tm_clones`, `register_tm_clones` and `__do_global_dtors_aux`,
and libgcc's double-precision and 64-bit division helpers, which Ubuntu
builds as Thumb-2. `BUILD-INFO` counts the `$t` mapping symbols per program.

Why musl: the Alpine musl busybox already runs on Ferrix armv7a, and a libc
that is neither kernel's own keeps libc quality out of a kernel comparison.
Not ferrousli, which is Ferrix's own libc, so Linux's figure would depend on
it; not static glibc.

## lmbench

### Licence

lmbench is distributed under the GPL version 2 "with the following
additional restrictions (which override any conflicting restrictions in the
GPL)", quoted from `COPYING-2` in the tarball:

> 1. You may not distribute results in any public forum, in any publication,
>    or in any other way if you have modified the benchmarks.
>
> 2. You may not distribute the results for a fee of any kind.  This includes
>    web sites which generate revenue from advertising.

The same file's rationale adds that "if you formally report LMbench results,
you have to report all of them and make the raw results file easily
available", that a paper "*must* also include the rest of the standard
lmbench numbers" (in an appendix if need be), and that the restrictions
"only apply to *publications*": use during development, and exchanging
results between OS developers, is unrestricted.

What that means here:
- **Development and internal comparison:** unrestricted.
- **A paper:** possible only with unmodified benchmarks, so the source is
  the original 3.0-a9, not a fork (intel/lmbench, which Buildroot uses,
  changes benchmark code, e.g. `lat_mem_rd`'s count). No lmbench file is
  changed: the build adds two empty headers outside lmbench's tree and the
  flag `-std=gnu89`, and is otherwise lmbench's own `scripts/build`.
- **The subset run here is not "the standard lmbench numbers".** By the
  rationale, a paper reporting it must also carry lmbench's full standard
  run (`scripts/lmbench`) for both kernels, and the raw results files. That
  run needs programs not built here (TCP, UDP, RPC, file system, `lmdd`,
  `stream`, `par_mem`, `tlb`); `lat_rpc` needs Sun RPC, which musl lacks.
- **Restriction 2:** results must not be distributed for a fee; whether a
  paywalled venue counts is for the customer to settle before submitting.

### What runs

`run-lmbench.sh [BIN_DIR]`, every program with `-P 1 -N 11` (one process,
the median of 11 samples, lmbench's own count) and `ENOUGH=100000` (100 ms a
sample, fixed so both kernels time alike; lmbench would otherwise choose its
own interval per kernel). lmbench's overhead corrections (`LOOP_O`,
`TIMING_O`) are measured by each program at start, as lmbench does when they
are not given.

| Tag | Command | Unit |
|---|---|---|
| `lat_syscall.null`, `.read`, `.write` | `lat_syscall null` (getppid), `read` (1 byte of /dev/zero), `write` (1 byte to /dev/null) | us |
| `lat_syscall.stat`, `.fstat`, `.open` | `lat_syscall stat|fstat|open $TMP/lmbench-stat` | us |
| `lat_pipe`, `lat_unix` | defaults | us |
| `lat_ctx.s0.p2`, `lat_ctx.s16.p2` | `lat_ctx -s 0 2`, `lat_ctx -s 16 2` | us |
| `lat_proc.fork`, `.exec`, `.shell` | `lat_proc fork|exec|shell`; `hello` is copied to `/tmp/hello` first | us |
| `lat_sig.install`, `.catch` | `lat_sig install|catch` | us |
| `lat_pagefault` | on an 8 MB file in `$TMP` (lmbench's default MB=8) | us |
| `lat_mmap.512k`, `.2m`, `.8m` | `lat_mmap <size> $TMP/XXX` (it refuses sizes under 320 KB) | us |
| `bw_pipe`, `bw_unix` | defaults | MB/s |
| `lat_mem_rd.s64.<MB>` | `lat_mem_rd 32 64`: every range from 512 bytes to 32 MB at the A7's 64-byte line | ns |
| `bw_mem.<rd|wr|cp>.<16k|128k|8m>` | L1, L2 and DRAM sizes | MB/s |

`exec-check` runs before `lat_proc`: `lat_proc` never reads its child's exit
status, so a kernel whose `execve` fails would still get a figure (fork plus
a failed exec). `exec-check` makes the same two calls once, and when one
fails, the matching `lat_proc` line is `FAIL status=exec-check`. Under
qemu-arm without binfmt, where the exec of an ARM program fails, it fired as
it should.

Output: `lmbench <tag> <value> <unit> round=<r>` or
`lmbench <tag> FAIL status=<s> round=<r>`, every line a program printed as
`lmbench-raw <tag> <line>`, a `lmbench-info` header (uname, settings, the
file system of `$TMP`) and `lmbench-done failures=<n>`. Environment:
`LMB_ROUNDS`, `LMB_TMP`, `LMB_ENOUGH`, `LMB_ONLY` (see the script).

### What the timed loops call

From `qemu-arm -strace` of each program (Linux semantics, ARM EABI numbers
checked against Linux's `asm-arm/unistd-common.h`):

| Tag | Timed call(s), through musl 1.2.5 |
|---|---|
| `lat_syscall.null` | `getppid` (64) |
| `lat_syscall.read`, `.write` | `read` (3), `write` (4) |
| `lat_syscall.stat`, `.fstat` | `statx` (397): musl's `stat` and `fstat` on 32-bit Arm try `statx` first and fall back to `fstatat64` (327) or `fstat64` (197) only on ENOSYS |
| `lat_syscall.open` | `open` (5), `close` (6) |
| `lat_pipe`, `lat_unix`, `lat_ctx`, `bw_pipe`, `bw_unix` | `read`, `write` |
| `lat_proc.fork` | `fork` (2), `exit_group` (248), `wait4` (114) |
| `lat_proc.exec` | the above and `execve` (11) |
| `lat_sig.install` | `rt_sigaction` (174) |
| `lat_sig.catch` | `kill` (37), the handler, `sigreturn` (119) |
| `lat_pagefault` | `mmap2` (192), `msync`, `munmap` (91), the faults |
| `lat_mmap` | `mmap2`, `munmap` |

Timing is `gettimeofday`, which musl implements with `clock_gettime`: on
Linux through the vDSO (`__vdso_clock_gettime64`), with no system call. On a
kernel without a vDSO each reading is `clock_gettime64` (403). lmbench reads
the clock twice per sample of `ENOUGH` microseconds and subtracts its own
measured timing overhead, so this moves no figure by more than the overhead
correction's error.

Where a figure depends on musl's wrapper rather than on one system call,
say so beside it: `stat`/`fstat` are `statx` only if the kernel has
`statx`; otherwise every call is two system calls.

## speedtest1

`run-speedtest1.sh [BIN_DIR]` runs `speedtest1 --size $ST_SIZE --testset
main` twice: with `--memdb`, and on `$ST_DIR/speedtest1.db`. `--testset
main` is given because 3.53.4's default is the `mix1` macro, which includes
the R-Tree tests. speedtest1 is compiled with the base options of SQLite's
own `test/speedtest.tcl` (`-DSQLITE_OMIT_LOAD_EXTENSION
-DSQLITE_THREADSAFE=0`) and musl's malloc, so the kernel's `brk` and `mmap2`
paths are part of what it measures; otherwise it runs with speedtest1's
defaults (journal mode DELETE, synchronous FULL, so the file case calls
`fsync`).

Output: `speedtest1 <mode>.<test> <seconds> s round=<r>` per test, from
speedtest1's own timing lines, `speedtest1 <mode>.total ...`, the raw lines
as `speedtest1-raw <mode> <line>`, and `speedtest1-done failures=<n>`.
speedtest1 times in milliseconds.

### The size

`ST_SIZE` defaults to 60, chosen so one run takes about 20 to 60 s on a
650 MHz Cortex-A7:
- **Measured** (qemu-arm 9.2.4 with QEMU's `insn` plugin, user-mode
  instructions only, `--testset main`): 1.70, 7.83 and 22.4 billion
  instructions at sizes 10, 40 and 100 with `--memdb`; the file database
  adds about 1 to 3 % in user mode, plus its system calls.
- **Interpolated**: about 13 billion at size 60.
- **Argued**: the A7 is in-order and partly dual-issue; at 0.4 to 0.7
  instructions a cycle on code like SQLite's, 13 billion at 650 MHz take 29
  to 50 s.

The first Linux run on the board confirms it. Then the size stays fixed for
both kernels.

### The file database on Ferrix

Ferrix's `/tmp` is a kernel tmpfs, mode 1777, and so is its root when
booted with `--tmpfs-root` (or `ferrix.root=tmpfs`); `/dev/shm` is a tmpfs
too. Ferrix has no ramfs (`mount -t ramfs` is ENODEV), and a btrfs root or
`/data` is a disk, not RAM. So the file case is a tmpfs on both kernels:
Linux's initramfs must mount a tmpfs at `/tmp` (its rootfs may be ramfs),
and `run-speedtest1.sh` prints the file system it found.

## Running them

Both images carry this directory's `bin/` and run scripts at one path, here
`/opt/posixbench`, and a busybox. Both need `/tmp` as a tmpfs, `/dev/null`
and `/dev/zero`, and `cp`, `chmod`, `dd`, `rm` and `uname`.

`lat_proc shell` execs `/bin/sh`, so its figure compares the two images'
shells as well as their kernels: the same `/bin/sh` (the Alpine static
busybox) is needed on both for a kernel ratio. On Ferrix, `/bin/sh` is zinc
whenever zinc is carried.

A Linux initramfs (`/init`, busybox):

```
mount -t proc proc /proc
mount -t tmpfs tmpfs /tmp
cd /opt/posixbench
LMB_ROUNDS=1 LMB_TMP=/tmp sh run-lmbench.sh /opt/posixbench/bin
ST_SIZE=60 ST_DIR=/tmp sh run-speedtest1.sh /opt/posixbench/bin
poweroff -f
```

A Ferrix image: the same two commands from a `ferrix.init=` script that
starts `#!/bin/busybox sh` (`#!/bin/sh` is zinc) and sets `PATH=/bin`,
`/tmp` being Ferrix's tmpfs already. Under QEMU on nazuna it was booted with
the files carried as a local app package:

```
cargo xtask run --arch armv7a --tmpfs-root \
  --init "$HOME/.local/share/ferrix/busybox/{arch}/bin/busybox.static" \
  --app posixbench --init-path /opt/posixbench/ferrix-run.sh --memory 1024
```
