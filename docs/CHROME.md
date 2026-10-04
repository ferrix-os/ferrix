# Chrome on Ferrix: what it would take

Asked by the customer on 2026-09-18, and answered by reading the tree rather
than by guessing. This is an assessment, not a decision and not a stage: what
a browser needs, what Ferrix has, what is missing, and what the missing part
would cost. Whether any of it is worth doing was the product owner's to say,
and on 2026-09-23 the customer asked for the work to start.

## Where it stands, 2026-09-26

**Chrome runs on ferrousli in glibc's place, since 2026-09-26.** The same
headless Chrome, its forty Debian libraries unchanged, runs on Ferrix on
ferrousli's `ld.so` and `libc.so.6` instead of Debian's loader and glibc,
and `cargo xtask test-chrome --interpreter ferrousli --library ferrousli`
requires the same three things of it -- the version, a page's script run by
V8, a screenshot -- with its GPU process drawing through SwiftShader as it
does on glibc (§8). **So does the full browser in a window, the same day:**
`cargo xtask test-chrome-window --interpreter ferrousli --library
ferrousli` requires its page on the compositor's screen, and still there
after a click and typing (§8).

**Chrome runs on Ferrix, in a window on the compositor, since 2026-09-24.**
`cargo xtask run-compositor --chrome` opens it on the desktop, and
`cargo xtask test-chrome-window` requires its page on the screen (§9).

**Headless Chrome runs on Ferrix, since 2026-09-24.** Google's prebuilt
`chrome-headless-shell` 154.0.8037.57 -- Chrome for Testing's linux64 build,
not one built here -- starts on Ferrix with its GPU process and renderers,
runs a page's JavaScript in V8 and writes a screenshot, and `cargo xtask
test-chrome` requires all three (§8). It is not on the image: it lives on a
btrfs volume `tools/common/fetch/fetch-chrome.sh` makes from pinned downloads, with
Debian 13's glibc and the forty libraries it loads. What it runs with, and
what is left:

| | state |
|---|---|
| Dynamic linking: a glibc program on ferrousli's loader and `libc.so.6`, `dlopen` and every TLS model | **done** 2026-09-23, on all three architectures (§2.1, §4) |
| Somewhere to put it: btrfs written from Ferrix | **done** 2026-09-21, stage 12 (§2.3) |
| `/dev/shm`, and `clone` refusing the namespaces it cannot give | **done** 2026-09-19, on `main` 2026-09-23 (§2.4, §3) |
| The compositor a window would appear in, drawing on the GPU | **done**, stage 19's GPU path (§1, §5) |
| `execve` of a binary past 64 MiB, mapped from the file on demand | **done** 2026-09-24, on all three architectures, with `execve("/proc/self/exe")` from a fork (§2.2) |
| `timerfd` | **done** 2026-09-24, on all three architectures, the first kernel row of §6's foot (§3) |
| `madvise` and `signalfd` | **done** 2026-09-24, on all three architectures (§2.3, §3) |
| A vDSO | **done** 2026-09-26, x86-64: `clock_gettime`, `gettimeofday` and `time` read the TSC in the program (§3, §9) |
| libwayland-client, libxkbcommon, fontconfig with freetype and expat, a font | **done** 2026-09-24, built against ferrousli with foot (§3, §6) |
| foot, a Wayland terminal nobody here wrote, drawing on the compositor on Ferrix | **done** 2026-09-24, x86-64, `cargo xtask test-foot` (§6) |
| Headless Chrome on Ferrix: `--dump-dom` and `--screenshot`, multi-process, with `--no-sandbox` (and `--no-zygote` until 2026-09-26) | **done** 2026-09-24, x86-64, `cargo xtask test-chrome` (§8) |
| What running it found missing: `CLOCK_THREAD_CPUTIME_ID` and `CLOCK_PROCESS_CPUTIME_ID`, `clock_getres`, `creat`, and `/proc/<pid>/task`'s link count | **done** 2026-09-24 (§8) |
| Chrome in a window on the compositor, a Wayland client drawing in software | **done** 2026-09-24, x86-64, `cargo xtask test-chrome-window`, `run-compositor --chrome` (§9) |
| The zygote, which could not learn its children's pids until `SCM_CREDENTIALS` carried them | **done** 2026-09-26: `test-chrome` (glibc and ferrousli) and `test-chrome-window` run without `--no-zygote` (§8) |
| Chrome's speed: a futex wait woken every 5 ms, the HPET as the clock under KVM, `munmap` walking every page of a reservation, a shootdown for every write to a page after a fork, and the virtio-gpu doorbell held by QEMU | **done** 2026-09-26: idle 443% of a processor → 15%, the machine 96% busy → 6%, a turning box 1.5 frames a second → 60.8; then a vDSO (idle → 13%) and a kick to one processor (QEMU on the host 81% of a core → 65%); `cargo xtask bench-chrome` (§9) |
| Chrome playing a video with its sound, every futex call in the system behind one lock | **done** 2026-09-26: the kernel's futex table in 256 buckets; Chrome's processor time a third lower and flat, video frames dropped 29% → 19%, three alternated runs of each at a host load of 25–39; the sound's remaining gaps follow the host's load; `cargo xtask bench-chrome-video` (§9) |
| Chrome on the desktop's persistent btrfs root | **done** 2026-09-26: `/dev/shm` was not mounted there; `cargo xtask test-chrome-window --btrfs-root` (§9) |
| Chrome's text, its name for the system, and Chrome for Testing's bar | **done** 2026-09-26: Inter and Liberation from the tree with slight hinting, Ferrix in the user agent, no bar; `navigator.platform` and the client hints are Chrome's build's and say Linux (§9) |
| Chrome on ferrousli's `libc.so.6` in glibc's place | **done** 2026-09-26, x86-64, headless and in a window: `cargo xtask test-chrome` and `test-chrome-window`, each with `--interpreter ferrousli --library ferrousli` (§8); `run-compositor --chrome` does not take the flags |
| Chrome on the STM32MP157D-DK1: an armhf Chromium, an SDMMC driver, page-cache eviction | not started, ≈ 45–55 points (§10) |
| Chromium built against ferrousli, with Alpine's musl patches rebased | not needed for a first Chrome: the prebuilt one runs (§5, §8) |
| A guest with the ~2 GiB a page wants | `test-chrome` boots 4 GiB, as `test-rustc` does (§2.3) |

**Done, 2026-09-24: a foreign toolkit client inside the guest** (§6).
`foot` 1.24.0, a real Wayland terminal nobody here wrote, runs on Ferrix's
compositor and draws a program's output in the image's font. It is built
by the foot port, now the `foot` app, against ferrousli with libffi 3.5.2,
wayland 1.24.0, wayland-protocols 1.45, libxkbcommon 1.11.0, pixman 0.46.4,
freetype 2.14.1, expat 2.7.3, fontconfig 2.17.1, tllist 1.1.0 and fcft
3.3.2, and DejaVu Sans Mono 2.37 is the font. **Then** headless Chrome,
the same day (§8), and a window (§9). **Then**, on 2026-09-26, Chrome on
ferrousli in glibc's place (§8), and the desktop's persistent btrfs root
(§9). **Then**, the same day, Chrome's speed, and its zygote (§8, §9), and
the full browser in a window on ferrousli (§8).

**Re-checked on 2026-09-19**, against a tree 37 commits further on. Everything
in §2, §3 and §4 still holds but the two loose fixes in §6, which are now
done, and §5's last paragraph, which was overtaken the day after it was
written. Each is marked where it stands. One of the two turned out not to cost
what this document said it did, which is recorded rather than quietly
corrected: see §3.

**Re-checked on 2026-09-23**, when the customer asked for this work to start.
Two of §2's four walls have fallen since: dynamic linking is done on all
three architectures, `dlopen` included (§2.1, §4), and btrfs is writable
(§2.3). §2.2, the rest of §2.3 and §3 stand as written; §5's first row is
now zero.

`docs/ROADMAP.md` stage 22 already names a browser once -- Steam's client
starts one as a helper -- but a browser of Ferrix's own is on no stage, and
nothing on the roadmap arrives at one on the way to something else.

**The short answer:** on the order of 120 to 170 points of new work, on top of
roughly 95 points that were already planned for other reasons when this was
written -- of which dynamic linking and btrfs write have since landed,
leaving stage 13's ≈ 60, and those only if the sandbox is wanted. That is Steam's shape and
Steam's size, and like Steam most of the total is discovered by running the
thing rather than by planning it.

---

## 1. What is already in a browser's favour

This is further along than the question usually starts from, and the reasons
are worth naming because each was built for something else and pays here.

* **The system-call surface.** About 237 of the 263 names `src/lib/proto/linux-abi`
  carries are answered with real work. The ones Chrome's process model stands
  on are among them: `clone`/`clone3` with real threads, `futex`, all six
  `epoll` calls, `eventfd2`, `memfd_create` with seals, and `AF_UNIX` with
  `SCM_RIGHTS` descriptor passing and full `cmsg` handling. That last one is
  Mojo, Chrome's own IPC, and it works today.
* **The C library.** `docs/POSIX-2024.md` counts 1045 of POSIX.1-2024's 1243
  interfaces present in ferrousli, none stubbed -- 1040 when this was
  written, and the five pseudo-terminal calls foot opens its terminal with
  since. Threads are complete down to
  robust and priority-inheriting mutexes and cancellation, and the thread
  control block keeps glibc's layout.
* **A C++ runtime that is exercised.** `src/user/system/linux/ferrousli/tools/ports/libcxx` builds
  LLVM 23.1.1's libc++, libc++abi and libunwind against ferrousli, exceptions
  and all, and btop 1.4.7 -- a C++23 program with threads -- draws its panels
  on Ferrix. curl fetches over HTTPS and git clones, both built the same way.
* **A compositor a toolkit will start against.** `src/user/system/linux/compositor/hyprix` advertises
  every global Chromium's Ozone backend binds: `wl_compositor`,
  `wl_subcompositor`, `wl_shm`, `wl_seat`, `wl_data_device_manager`,
  `xdg_wm_base` at 6, `zxdg_decoration_manager_v1`, `wp_viewporter`,
  `wp_fractional_scale_v1`, `zwp_text_input_v3`, `wp_presentation`, and the
  relative-pointer and pointer-constraints pair. The `wl_shm` buffer path needs
  nothing this compositor has not got.

---

## 2. The four things that actually stop it

### 2.1 Dynamic linking

Chrome is a position-independent executable linked against glibc, and it
`dlopen`s more at run time. **Done, 2026-09-23:** a program linked against
glibc runs on ferrousli's loader and `libc.so.6` in glibc's place on all
three architectures, with `dlopen`, `dlsym` and the rest of `dlfcn.h` and
every TLS model (§4). One limit was Chrome's to meet: a library `dlopen`ed
after start-up could not have a `PT_TLS` of its own, and ANGLE's are the
kind a browser opens late. **Met 2026-09-26** (§8): such a library's block
goes in a surplus of static TLS kept at start-up, as glibc's does. A fully static Chromium against a musl-shaped library
is not a configuration anybody ships: Alpine, which is the only distribution
that builds Chromium against musl at all, builds it dynamically and carries a
patch set to do it.

This is the 39-point section `docs/ROADMAP.md` already stages, and §4 below
says how much of it is done. It is unavoidable on either route.

### 2.2 The exec path cannot load a binary this size

**Done, 2026-09-24.** `execve` does not read the program any more. It opens
the file and reads its first page -- a script's `#!` line, or an ELF file's
header and program header table, read on to the table's end if that is
further -- and maps the segments from the file's page cache, the same object
an `mmap` of the file maps (`src/kernel/src/syscall/program.rs`,
`src/kernel/src/syscall/load.rs`). Every page wholly inside one segment's file
contents is a private mapping of the file: read from the disk the first time
the program touches it, shared by every process running the same program,
and copied into the process's own object the first time it is written, so a
writable segment never writes its file. The partial pages at a segment's
two ends, a page two segments share and `.bss` are anonymous and are copied
or zeroed as before. A file with no object to map, and a program built into
the kernel, are still copied, a piece at a time rather than whole. The linker
`PT_INTERP` names is loaded the same way; the libraries it maps never had the
limit, since `mmap` of a file always mapped its object. What is left of a
limit is memory for page tables.

The boot check (`src/kernel/src/fs/exec_check.rs`, FX-0871) loads a 72 MiB
program whose file is served by a page source that counts what it is asked
for, as btrfs serves its page cache from disk. On every boot it reports
`a 72 MiB program was loaded reading 3 of its 18437 pages, 67 by the time it
had been touched and had run`: the headers' page, the data segment's and the
partial page the large segment ends on; then one run of 32 for a read 40 MiB
in, and one for a write 50 MiB in, which must read back while the file's page
keeps its byte. The bytes past the segment's file contents must be zeros
though the file's are not, and the program must run to its status. Made to
read the whole file again, the check fails with `loading read 18437 of the
program's 18437 pages`.

**`/proc/self/exe`, the same day.** Chrome starts every child process -- the
GPU process, the network service, each renderer -- by forking and running
`execvp("/proc/self/exe")`, and that failed with `ENOENT`, which Chrome
reports as `LaunchProcess: failed to execvp: /proc/self/exe` and then `GPU
process isn't usable. Goodbye.` Two things were missing. A fork child did not
inherit what its parent was started as, so its `/proc/self/exe` had no
target at all; and the link was followed as its text, so a program whose file
had been renamed or deleted could not be run again through it. A fork now
takes its parent's identity (`Process::forked`), and `/proc/<pid>/exe` is a
magic link, as on Linux: followed, it leads to the file the program was
loaded from, by `Inode::link_location` (`src/lib/fs/vfs/src/walk.rs`), whatever its
name is now; read, it is that file's path with ` (deleted)` after a file since
removed. The boot check forks the sparse 72 MiB program, removes its name,
and has the fork `execve("/proc/self/exe")`, which must run it to its status.
With the fork's identity left out, as before, it fails with `errno 2`, and
with the link followed as text, with `errno 2` again.

**On Chrome, 2026-09-24**, x86-64 under KVM with 2 GiB, the 198 MB
`chrome-headless-shell` 154.0.8037.57 on the btrfs volume
`tools/common/fetch/fetch-chrome.sh` makes (branch `chrome/headless`), run by `cargo
xtask test-chrome`:

* on Debian's `ld-linux` and glibc, `execve` of the 198 MB file succeeds, the
  linker maps its libraries and Chrome runs, until PartitionAlloc's first
  `madvise` is refused with `ENOSYS`. Its `CHECK` executes `int3`, which a
  program on Ferrix is ended for with `SIGSEGV`: `pid 215 ended by signal 11
  at 0x0, pc 0x55555726974d`, the `int3` after `madvise@plt` at
  `0x1d1474d`. With `madvise` answered 0, a trial kernel that is not landed,
  it prints `Google Chrome for Testing 154.0.8037.57`; `--dump-dom` then
  starts its child processes through `/proc/self/exe` with no `execvp`
  failure, and two of them end at a `CHECK` of their own
  (`+0x3f11f11`) while the browser ends reading a null pointer in libc.
  Those are the next track's, not this one's;
* on ferrousli's `ld.so` and `libc.so.6` in glibc's place, with the volume's
  other libraries on `LD_LIBRARY_PATH`, the program is loaded and the loader
  stops at the first glibc name ferrousli lacks: `ld-ferrousli: undefined
  symbol: program_invocation_short_name`.

Neither stops at `execve`. What is deliberately not done: `ETXTBSY`, so a
running program's file can be written, and the program sees the new bytes
in pages it has not yet copied, where Linux refuses the write; the partial
pages at a segment's ends are copied, not mapped, so `/proc/<pid>/maps` shows
a segment as its file's pages with an anonymous page either side, where
Linux shows one run; and `/proc/<pid>/cwd`, `root` and `fd/<n>` are still
followed as their text.

What follows is how it stood before.

`execve` reads the whole file through `fs::read_file`, whose `READ_FILE_LIMIT`
is 64 MiB (`src/kernel/src/fs/mod.rs`), and `load.rs` maps the segments and copies
the image in. There is no demand-paged, file-backed executable mapping and no
page cache behind `execve`. Chrome's binary with its resources is 180 to 250
MiB.

This is the same wall btop hit at 4.6 MiB one order of magnitude up, and the
fix is a different one: btop was cured by reading into a `vmap::Buffer` instead
of one heap block, and this needs the pages to arrive on fault from the file.

### 2.3 Nowhere to put it, and not enough memory

The root filesystem is the initramfs, which is RAM; btrfs was read-only
until stage 12, which landed on 2026-09-21, so there is now somewhere to put
a browser that is not memory; the guest is 512 MiB by default. Chrome wants something like 2 GiB to
open one page. And `madvise` decodes but has no handler, so PartitionAlloc and
V8 could never give memory back -- on a guest this size that is the difference
between slow and dead.

**`madvise` done, 2026-09-24** (§3): `MADV_DONTNEED` and `MADV_FREE` now give
a range's frames back to the allocator at once and leave it mapped, so the
next touch reads zeros. The memory is still 512 MiB against Chrome's 2 GiB;
what changed is that an allocator which gives pages back actually gets them
back.

### 2.4 The sandbox, and a bug worth fixing whatever is decided

Chrome's zygote wants user, pid and mount namespaces and a seccomp-bpf filter.
Ferrix has neither: `unshare` refuses everything it cannot honour
(`src/kernel/src/syscall/namespace.rs`), and there is no `seccomp` or `bpf` number
at all. Running `--no-sandbox` is the honest first target; stage 13 is the
other answer.

**Done, 2026-09-19:** `clone` and `clone3` refuse the `CLONE_NEW*` flags with
`EINVAL`, as `unshare` always did and as a Linux built without `CONFIG_*_NS`
does. A ring-3 program on all three architectures proves it. What follows is
what the state was, and why it mattered.

But `clone` and `clone3` did not check the `CLONE_NEW*` flags at all.
`src/lib/proto/linux-abi` defines them -- `CLONE_NEWNS`, `CLONE_NEWUSER`, `CLONE_NEWPID`
and `CLONE_NEWNET` are all in `types.rs` -- and no line of
`src/kernel/src/syscall/family.rs` ever tested one: `clone_with` checked the
`CLONE_THREAD`, `CLONE_SIGHAND` and `CLONE_VM` combinations and `CLONE_PIDFD`,
and nothing else. So `clone(CLONE_NEWUSER|CLONE_NEWPID|SIGCHLD)` succeeded and
handed back an ordinary child in the one namespace there is. A program that
asked for a sandbox was told it got one. `unshare` was honest about exactly the
same request, which made it an inconsistency inside the kernel as well as a lie
to the caller. It was a small fix and it was worth making on its own account.

It changes nothing about §2.4's real answer: `--no-sandbox` is still the
honest first target, and stage 13 is still the other one. What it changes is
that a program which asks for isolation now finds out it cannot have it.

---

## 3. The smaller things, each of which would bite

* ~~**No vDSO.**~~ **Done, 2026-09-26, on x86-64.** `AT_SYSINFO_EHDR` was
  absent, so every `clock_gettime` was a trap, and Chrome calls it per task,
  per timer and per trace point: forty thousand times a second. Every
  program now gets Linux's shape of vDSO, `linux-vdso.so.1` with
  `__vdso_clock_gettime`, `__vdso_gettimeofday` and `__vdso_time` at
  `LINUX_2.6`, over a data page holding the TSC's frequency and the
  real-time offset (`src/lib/kernel/vdso`, `src/kernel/src/syscall/vdso.rs`). Under an
  emulator, whose clock is the HPET, the functions make the system call.
  AArch64 and ARMv7-A have none yet.
* ~~**No `madvise`.**~~ **Done, 2026-09-24.** The number decoded and nothing
  answered it, so PartitionAlloc and V8 (§2.3) could hand memory back and
  never get it. `MADV_DONTNEED` and `MADV_FREE` now drop a range's pages and
  leave it mapped: private anonymous memory reads as zeros on its next touch,
  a private file mapping as its file, and a shared mapping keeps its contents,
  as Linux's do. The frames go back to the allocator in `madvise` itself, in
  the order every unmap here keeps -- translations down under the address
  space's lock, one shootdown with the lock let go, and only then the frames.
  `MADV_FREE` is allowed to keep its pages until memory is short, and here
  drops them at once, as Linux does with no swap to age them against.
  `MADV_REMOVE` punches a hole in shared anonymous memory; on a file it is
  `EOPNOTSUPP`, as `fallocate`'s hole is here, which Chrome's discardable
  memory on a memfd only logs. The hints are accepted where Linux accepts
  them, and `MADV_WIPEONFORK` is refused rather than accepted and ignored,
  since BoringSSL keys its reseeding on it. The boot check counts the frames:
  eight written pages dropped must come back as exactly eight frames, on all
  three architectures.
* ~~**No `/dev/shm`.**~~ **Done, 2026-09-19**, and it was not one line, which
  is what this said. Chromium's shared memory prefers `memfd_create`, which
  exists with seals, but falls back to `/dev/shm`, and ferrousli's named
  semaphores live there too. Adding the directory to the initramfs would have
  done nothing: the boot mounts devfs on `/dev` and that shadows whatever the
  archive unpacked there. devfs cannot create a name and has no storage, so
  `/dev/shm` has to be a tmpfs mount — and nothing could be mounted anywhere
  inside `/dev`, because devfs does not cache lookups and a mount point is a
  cached dentry. That took a VFS change, `Inode::caches_lookup_of`, which lets
  a directory of coming-and-going names keep the one name that never changes.
  Roughly 200 lines with its two checks rather than one. The lesson is the
  usual one: a cost this document calls trivial is the kind most worth
  checking before it is quoted.
* ~~**No `timerfd` and no `signalfd`.**~~ **Both done, 2026-09-24:
  `timerfd` first, `signalfd` the same day.** Chrome's and glib's event
  loops take `SIGCHLD` and `SIGTERM` through a `signalfd` in their epoll
  set: the signals are blocked, and a read takes each one pending as a
  `signalfd_siginfo`. `signalfd4` answers on all three architectures and
  `signalfd` on the two that have it, with `SFD_NONBLOCK` and
  `SFD_CLOEXEC`, and a read takes the reading thread's signals and then
  its process's, as `rt_sigtimedwait` does. The waking wanted the same
  care as `timerfd`'s, for a different reason: a signal a thread blocks
  wakes nobody here, so a blocked read, `poll` or `epoll_wait` would have
  learned of it only at its own recheck a second later. Every process now
  has a queue its signals wake as they become pending, Linux's
  `signalfd_wqh`, and the boot check requires that wake, not the recheck,
  to end each of the three waits; they came back within 90 us of the
  signal on x86-64, 102 us on AArch64 and 73 us on ARMv7-A (71 us at two
  processors). ferrousli's
  `signalfd` wrapper landed with it. As for `timerfd`: foot, §6's first
  client, calls `timerfd_create`,
  `timerfd_settime` and `timerfd_gettime` about 45 times: its cursor blink,
  its flash, a delayed render and key repeat. All three are answered on all
  three architectures, with the `time64` forms on ARMv7-A, on
  `CLOCK_MONOTONIC`, `CLOCK_REALTIME` and `CLOCK_BOOTTIME`, with
  `TFD_TIMER_ABSTIME` and `TFD_TIMER_CANCEL_ON_SET`. The thing worth
  checking was not the counting but the waking: a `poll` or `epoll_wait` on
  a timerfd sleeps up to a second between looks of its own, so a timer that
  became readable only when somebody looked would blink a cursor a second
  late. A kernel thread, `timerfds`, sleeps until the earliest deadline and
  wakes the timer's waiters there. The boot check arms a timer only once a
  blocked read, a `poll` and an `epoll_wait` are each waiting on it, and
  requires the thread's wake to end each wait within a quarter of that
  second. In the seven boots made for it on example's QEMU -- x86-64,
  AArch64, and ARMv7-A at four processors and at two -- no waiter came back
  more than 3.5 ms after its deadline, and most within half a millisecond.
  What is not as Linux has it is in `docs/ROADMAP.md`, stage 17.
* **No AVX.** `src/kernel/src/arch/x86_64/switch.rs` saves a 512-byte `FXSAVE`
  area -- x87 and SSE -- and `CR4.OSXSAVE` is never set, so `CPUID` reports no
  OS support and V8 and Skia fall back to SSE2. That is correct rather than
  corrupting, and it is slow.
* ~~**Four C ports that do not exist.**~~ **Done, 2026-09-24,** for foot
  (§6): libwayland-client and libxkbcommon, which Ozone links; and
  fontconfig with freetype and expat, plus an actual font on the image. They
  are static libraries in foot's build today; Chromium, built dynamically,
  will want them as shared ones. The compositor still draws its own text
  with the coverage cells the term app carries; a client brings its own
  fonts, which is what these are for. Note that `src/user/system/linux/compositor/README.md`'s "no
  C device stack, ever" is a rule about the compositor, not about its
  clients.
* ~~**No audio at all**, which a browser survives and a person notices.~~
  **Done, 2026-09-26** (`docs/AUDIO.md`): Chrome's audio service falls back
  to ALSA, since there is no libpulse on the volume, and plays through
  alsa-lib's `default`, which is `plug` over `/dev/snd/pcmC0D0p`. The volume
  now carries alsa-lib's configuration (`libasound2-data`, linked at
  `/usr/share/alsa`), and `window_command` passes
  `--alsa-output-device=default`, `--audio-buffer-size=960` and
  `--autoplay-policy=no-user-gesture-required`. `cargo xtask
  test-chrome-audio` requires a page's 440 Hz tone in the file QEMU writes;
  `run-compositor --chrome --audio pipewire` plays it aloud, from the welcome
  page's button. **Since 2026-09-27 it goes through `pulsed`** (`docs/AUDIO.md`,
  U2d): the volume carries Debian's libpulse, which Chrome takes over ALSA
  once it loads, the desktop runs `pulsed` as a unit, and hyprix's clients
  are told `PULSE_SERVER`. Chrome plays 48 kHz stereo float, which the
  server mixes; `test-chrome-audio` requires its stream in `pulsed`'s log as
  well as the tone in QEMU's file. A volume fetched before then has no
  libpulse; xtask then leaves `pulsed` out and says so, and the gate refuses
  it. The gate runs Chrome on ferrousli, as the desktop does, unless
  `--interpreter glibc` asks for the volume's own; on ferrousli libpulse
  needed `backtrace_symbols` and a mutex inheriting priority refused as
  glibc refuses it (`docs/AUDIO.md`, "On ferrousli").

---

## 4. Where dynamic linking and btrfs write actually stand

**Both are done.** Dynamic linking met its exit on all three architectures
on 2026-09-23: Debian's glibc busybox runs on ferrousli's loader and
`libc.so.6` with nothing of glibc on the image, after ferrousli itself was
ported to AArch64 and ARMv7-A. btrfs write, stage 12, landed on 2026-09-21.
`docs/ROADMAP.md` has both. What follows is the history of this section.

Checked on 2026-09-18, because both are prerequisites above and both were
believed to be further along than they are.

**Dynamic linking: about 5 of the 39 points are on `main`.** `ffe7b264` places
an `ET_DYN` image with no interpreter at `PIE_BASE` and moves the entry,
`AT_PHDR` and the heap with it, with a boot check that loads a synthetic static
PIE on all three architectures. It landed for the threads work, because rustc's
default musl x86-64 target is a static PIE. `f7779ab5` is documentation: it
staged the 39-point plan across the roadmap, the backlog and the model, and
changed no code.

**Overtaken on 2026-09-20:** `PT_INTERP` is loaded now. `execve` places the
linker the program names at `INTERP_BASE`, enters it, and fills `AT_BASE`;
`src/lib/platform/elf` reads the path. That is 3 of the kernel half's 5 points, so **31
remain**, and what remains is the part this paragraph already said was the
expensive one -- there is still no loader anywhere. What follows is how it
stood on 2026-09-18.

**Overtaken again on 2026-09-21: 27 of the 39 are done, 12 left.**
`src/user/system/linux/ferrousli/ld` is a working loader -- symbol versions, `COPY`, initial-exec
TLS, `DT_FINI` -- and `src/user/system/linux/ferrousli/tools/build-shared.sh` links ferrousli as a
versioned `libc.so.6`. Debian's own dynamic busybox runs on Ferrix with
glibc's loader on all three architectures, and with ferrousli's in glibc's
place on x86-64. What Chrome still needs of it is most of the 12:
`dlopen` and the rest of `dlfcn.h`, and general-dynamic TLS, which a
`dlopen`ed library uses. `docs/ROADMAP.md`'s dynamic-linking section is the
current account.

What was not done is `PT_INTERP`, and the hook for it is one place --
`load.rs`'s refusal. `src/lib/platform/elf` already parses the relative relocations a
static PIE carries and reads symbol tables; ferrousli has the load-bias
arithmetic, a real `dl_iterate_phdr` and a real `dladdr`, which is what makes
C++ unwinding work. What does not exist anywhere, on any ref, is a loader: a
search of every commit in the repository for `PT_INTERP`, `DT_NEEDED`,
`JUMP_SLOT` and GNU-hash code finds only musl's vendored
`src/user/system/linux/ferrousli/include/elf.h`. **34 points remain**, not 39.

*(btrfs write is overtaken too: stage 12 landed on 2026-09-21, and the
roadmap has how it stands. What follows is 2026-09-18.)*

**btrfs write: no code, on any ref.** Searches across every commit for
`delayed_ref`, a transaction commit and the free-space tree return nothing.
`src/lib/fs/btrfs` says in its own header that it "knows nothing about transactions
or allocation", and `src/lib/fs/btrfs-vfs` answers `EROFS` from `write_at`,
`set_len` and `create`.

What is there, and it is the expensive half, landed with stage 11: the page
cache (`ffc95eac` and `f36551cf`, a file's VMO filled from a `PageSource`),
file-backed `MAP_SHARED` writing through to the file (`c63167ee`), and a block
stack that can already write -- `src/lib/drivers/block/virtio-blk` has `Write` and `Flush`
request types and `src/lib/proto/blkring` copies write payloads in. Writing a sector is
plumbed end to end and nothing above it uses that path. Three recorded
decisions park log-tree replay, eviction and writeback here.

Neither item has an owner: `docs/BACKLOG.md`'s owners table has both in the
`open` row.

One cleanup this turned up: `stage8-filemmap`, `vmo-map-wip` and `757065aa` on
`origin` look like pending work and are not -- their subjects duplicate commits
already on `main`. They are rebase leftovers.

---

## 5. What it would cost

Sizes are in the roadmap's currency and are first guesses, not an owner's
estimate. The first two rows are wanted for other reasons and are counted
separately for that reason.

| what | points |
|---|---|
| ~~**Already planned:** dynamic linking, the rest of it (34 when this was written; 12 on 2026-09-21)~~ *done 2026-09-23* | ~~12~~ |
| ~~**Already planned:** stage 12, btrfs write~~ *landed 2026-09-21* | ~~≈ 60~~ |
| **Already planned, only if the sandbox is wanted:** stage 13 | ≈ 60 |
| Demand-paged file-backed `execve`, and binaries past 64 MiB | 13 |
| ~~A vDSO; `madvise` and `signalfd` were sized with it~~ *done: `/dev/shm` 2026-09-19, `timerfd`, `madvise` and `signalfd` 2026-09-24, the vDSO on x86-64 2026-09-26* | ~~≈ 10~~ |
| ~~libwayland-client, libxkbcommon, fontconfig with freetype and expat, a font~~ *built with foot, 2026-09-24* | ~~13~~ |
| The Chromium cross-build against ferrousli, with Alpine's musl patches rebased | 40+, mostly unknown |
| What running it finds missing | unsized, ≥ 40 |

The GPU path of stage 19 (52 points, `docs/GPU.md`) is not required and is the
difference between a browser and a slideshow. It began on 2026-09-18, while
this was being written, and **it landed on 2026-09-19**: steps 1, 2, 3b and 5
are done, the desktop composites on the GPU, and a 1080p frame went from 39 ms
to 12 ms (`docs/GPU.md` §3.7 and §3.8). The sentence that stood here -- "the
compositor still draws every pixel on the CPU" -- was true for one more day.
So the row is not a cost a browser would have to carry; it is already paid.

---

## 6. The order worth doing it in

Two milestones before the browser, each of which is worth having on its own.

**First, a foreign toolkit client inside the guest.** `foot` is already the
compositor's real-client probe, but `src/user/system/linux/compositor/hyprix/probe/real-client.sh`
says in its own header that it is a development-host check: no client that was
not written against this tree's crates has ever run *on* Ferrix. Building
libwayland-client and libxkbcommon against ferrousli and running foot in the
guest proves the client story end to end for a fraction of a browser's cost,
and it is on the browser's path rather than beside it.

**Done, 2026-09-24.** `cargo xtask ports` builds foot and the ten libraries
under it statically against ferrousli (the foot port, now the `foot` app), and
`cargo xtask test-foot` boots the compositor with foot running `hyprctl
version`. foot finds DejaVu Sans Mono through fontconfig, lays out a 7x13
grid, starts four render threads and draws the three lines, which came
through a ferrousli pseudoterminal. The test requires foot's own account
of the font and the grid, no error from it, and antialiased text on the
virtio-gpu's screen with the compositor's background around it. What it
took beyond the ports was small, and each piece was found by linking or
running rather than by reading:

* the kernel's `timerfd` (§3);
* nine names in ferrousli: the three `timerfd` wrappers, `posix_openpt`,
  `grantpt`, `unlockpt` and `ptsname`, `fallocate`, and `eaccess`, which
  libxkbcommon checks its include paths with;
* a user in `/etc/passwd`, which foot looks up for its shell even when it
  is given a program, and `/var/cache/fontconfig`, which fontconfig will not
  make itself.

What it still says, and none of it stops it: `fallocate` cannot punch a
hole in a memfd, so foot's buffer pool keeps pages it could give back;
libxkbcommon finds no `/usr/share/X11/xkb`, which it does not need while the
compositor hands it the keymap; and foot is ported to x86-64 only. It was
first run against the compositor on example's own kernel, which proved the
static build a working Wayland client before the kernel had `timerfd`.
One thing the test's first version got wrong is worth keeping: it counted
colours on QEMU's default console, which was the firmware's text screen,
and passed. It reads the virtio-gpu now, and requires the compositor's
background, which the firmware's screen does not have.

**Then headless Chrome, not a window.** `chrome --headless --screenshot` drops
the compositor, the GPU, input and the whole font-theme surface, and still
exercises everything genuinely hard: the multi-process model, Mojo over
`AF_UNIX`, hundreds of threads, PartitionAlloc and V8. If a headless Chrome
writes a PNG on Ferrix, the Wayland half afterwards is comparatively small,
because §1 says the compositor is already ready for it.

And two fixes that stood on their own merits, whatever is decided about any of
the above: mount `/dev/shm`, and make `clone` refuse `CLONE_NEW*` rather than
ignore it. **Both are done, 2026-09-19.** They were the only part of this
document that was worth doing before anyone decides whether a browser is
wanted, because neither needs a browser to be worth having: the first is where
ferrousli's named semaphores already live, and the second was the kernel
telling a caller something untrue.

---

## 7. What was checked, and what was not

Read for this: `src/kernel/src/syscall/` against the `Syscall` enum, `src/lib/platform/elf`,
`src/lib/fs/btrfs` and `src/lib/fs/btrfs-vfs`, `src/lib/drivers/block/virtio-blk` and `src/lib/proto/blkring`,
`src/user/system/linux/ferrousli/src` and `src/user/system/linux/ferrousli/tools`, `src/user/system/linux/compositor/server` and
`src/user/system/linux/compositor/hyprix`, `tools/common/xtask/src/initramfs.rs`, and the history of every ref for
the two prerequisites in §4. Chromium's own requirements were taken from
Alpine's `community/chromium` APKBUILD and its musl patch set, which is the
only evidence that a Chromium against a musl-shaped C library builds at all.

Read again on 2026-09-19, for the re-check at the top: `src/kernel/src/fs/devfs.rs`
and `src/lib/fs/vfs`'s dentry cache, for what `/dev/shm` actually costs;
`src/kernel/src/syscall/family.rs` again; and, for §4, that no loader has appeared
(`DT_NEEDED` and `JUMP_SLOT` are still in no Rust in the tree), that
`src/lib/fs/btrfs-vfs` still answers `EROFS`, and that `madvise`, `AT_SYSINFO_EHDR`,
`timerfd` and `signalfd` are all still where §3 left them.

Not checked, and each could move the numbers: whether Chromium's build system
can be pointed at a sysroot shaped like ferrousli's without a patch of its own;
how much of Alpine's patch set applies to a library that is closer to glibc
than to musl; and what a 512 MiB guest does to a program that expects to be
told how much memory it has. None of those is answerable without trying it,
which is the honest reason the last row of §5 is unsized.

---

## 8. Headless Chrome on Ferrix, 2026-09-24

The customer chose, on 2026-09-24, to run Google's prebuilt Chrome first
rather than build Chromium: a first result in days rather than in the
40-plus points §5 priced a source build at, most of them unknown. Chrome
for Testing publishes `chrome-headless-shell` for linux64: a 198 MB
position-independent glibc program, with ANGLE and SwiftShader beside it,
that loads forty of the system's libraries -- glib, NSS, D-Bus, the X11
client libraries, gbm, udev, ALSA and what those load.

**Where it runs from.** `tools/common/fetch/fetch-chrome.sh` puts it on a btrfs volume
with Debian 13's glibc and loader and the Debian packages of those forty
libraries, fontconfig's configuration and DejaVu, every download pinned by
its SHA-256. After unpacking, it looks up every library each ELF file on the
volume needs, which found two -- `libcap` and `libsqlite3` -- that Debian's
builds need and the development host's did not. The same volume ran Chrome
on example's own kernel, through a copy whose `PT_INTERP` named the volume's
loader, before it was tried on Ferrix. It is not on the image: 809 MiB.

**What `cargo xtask test-chrome` requires**, booting 4 GiB with the volume at
`/data`: `--version`; `--dump-dom` of a page whose script writes `6*7` into
an element, so that the DOM holds `computed 42`, which only V8 could have
put there; and `--screenshot` of a page to a PNG on the disk. The first run
that passed wrote 4796 bytes.

**What running it found**, in the order it was found, each by booting it and
reading where it stopped:

1. `execve` read the whole file, and stopped at 64 MiB (§2.2). Done: mapped
   from the file on demand.
2. PartitionAlloc's `madvise(MADV_DONTNEED)` was `ENOSYS`, and Chrome's
   `CHECK` on it ended the process. Done.
3. Every child -- GPU process, network service, renderer -- is started by
   `execve("/proc/self/exe")`, which was `ENOENT` in a fork. Done.
4. Chrome's time code reads `CLOCK_THREAD_CPUTIME_ID` and `CHECK`s that it
   answered; it was `EINVAL`. Done: a thread's clock is its scheduler task's
   run time, charged up to the instant it is read, and a process's the sum
   of its threads'. Not as Linux: a thread that has ended takes its time
   with it.
5. The sandbox's thread helper counts threads by the link count of
   `/proc/self/task`, two and one per thread on Linux, and `CHECK`s it; it
   was two. Done.
6. `creat`, which the headless shell writes its screenshot with, and
   `clock_getres` were `ENOSYS` on x86-64 and ARMv7-A. Done.
7. The zygote, which Chrome forks its children from, said it could not
   fork, and none of its children started. Done, 2026-09-26: the zygote
   forks a child, and the child says hello on a socket whose reader set
   `SO_PASSCRED`. The zygote learns the child's real pid from the
   `SCM_CREDENTIALS` message the kernel attaches, and Ferrix attached none,
   so the zygote gave the child up. A unix socket message now carries its
   sender's pid and ids when its reader asked for them, or when the sender
   named them. `/proc/<pid>/oom_score_adj`, which the browser sets for
   each renderer, is kept and reported but acted on by nothing. The tests
   ran with `--no-zygote` until then, which made the browser `execve` the
   294 MB program afresh for every child; they no longer do. `--no-sandbox`
   remains, because Ferrix has no namespaces or seccomp (§2.4).

A trial kernel that let `madvise` answer and raised the read limit, never
landed, is how the later items were found before the earlier were fixed;
`--single-process` did the same for the children before item 3 was.

**What it still says, and none of it stops it:** `pkey_alloc`, `landlock_create_ruleset` and
`rseq` are `ENOSYS`, which Chrome and glibc take in their stride; and there
is no D-Bus or udev to talk to.

**Chrome on ferrousli.** Standing ferrousli's `libc.so.6` in for Debian's is
the other route, and the one this document started from. Measured on
2026-09-24: Chrome and its forty libraries import 730 glibc names, and 97 of
them are not in `libferrousli.a` -- the `_chk` fortify family, the old
`__xstat64` entry points, `iconv`, gettext's `textdomain` family, `fts64`,
`nftw64`, `statx`, `pidfd_open` and the rest. Counted by name and version
against the shared library it was 159, the rest hidden by the link or
exported only at glibc's newest version where Chrome, built against 2.31,
asks for older ones. All of them are answered since 2026-09-24; the same
volume and test are to run Chrome on ferrousli's loader next.

**Run on 2026-09-26, and passing.** `cargo xtask test-chrome --interpreter
ferrousli --library ferrousli` carries ferrousli's loader at the path
Chrome's `PT_INTERP` names and ferrousli as `/lib/libc.so.6`, with
`LD_LIBRARY_PATH=/lib:/lib/x86_64-linux-gnu`, so that everything else is
the volume's. The first run that passed printed the version, `computed
42`, and wrote a 4796-byte screenshot, with no ANGLE or Vulkan error. Each
stop was found by running Chrome on example's own kernel first, through a
copy whose `PT_INTERP` names ferrousli's loader, then on Ferrix. In the
order they were found:

1. **Twelve names the count above missed.** It said every glibc name was
   answered; the loader stopped at `getttynam`. libblkid and libmount, which
   GLib's GIO loads, want BSD's `err` family and the `ttyent` calls, and
   the window build's CUPS, GMP, GnuTLS and libunistring want `lockf`,
   `__strlcpy_chk`, obstacks and `pthread_rwlockattr_setkind_np`; with the
   new mount API's `fsopen` family, all are in ferrousli now, with
   `tests/c/glibc/libraries.c`. Recorded rather than corrected: the count
   was wrong, and the loader, not a list, is what finds the last ones.
2. **glibc's own `libm.so.6` loaded beside ferrousli.** The loader used
   glibc's file of a split-off name where one was on the path, and the
   volume has them all; `libm`'s first ifunc resolver read the processor's
   features through `_rtld_global_ro`, which only glibc's loader defines,
   and faulted. `libm.so.6`, `libpthread.so.0` and the rest are now always
   answered by ferrousli's `libc.so.6`.
3. **A library's reference to its own versioned symbol.** libgcc_s's
   constructor calls its own `__cpu_indicator_init@GCC_4.8.0`; the index
   such a reference carries is one of the object's version *definitions*,
   and the loader looked only among its *needs*.
4. **`dlsym(RTLD_NEXT, "close")`.** Chrome defines `close` itself and
   finds the C library's this way; the loader refused `RTLD_NEXT`. libc's
   `dlsym` now passes its return address to the loader (interface
   revision 4), which searches the objects after the caller's.
5. **`locale_t`'s layout.** libc++ built against glibc takes `ctype<char>`'s
   table from `newlocale`'s C locale, reading `__ctype_b` 104 bytes in;
   ferrousli's locale was 48 bytes of its own. It is glibc's
   `struct __locale_struct` now.
6. **Chrome's own `malloc`.** Chrome replaces the allocator with
   PartitionAlloc, and every other object's calls bind to it; ferrousli's
   own, being Rust calls, did not, so `strdup`'s memory came from
   ferrousli and was freed into PartitionAlloc, which `CHECK`ed. The
   library now calls the program's `malloc`, `free`, `calloc` and
   `realloc` where it has them, as glibc does -- except for its fork
   handlers, which PartitionAlloc registers holding its own lock, and
   which a call back into it waited on for ever.
7. **GNU's `strerror_r`.** glibc's `strerror_r` returns the message;
   ferrousli's, POSIX's, returned 0, and Chrome traps on a null message.
   GLib, fontconfig, systemd and p11-kit ask for GNU's too. `libc.so.6`
   exports GNU's under the name; the static library keeps POSIX's.
8. **TLS in a `dlopen`ed library.** The GPU process opens four with TLS --
   ANGLE's EGL and GLES, the Vulkan loader and SwiftShader -- and the loader
   refused them, so ANGLE had no Vulkan, the GPU process fell back, and on
   Ferrix the fallback stopped at the sandbox's
   `proc_util.cc:115` check with `ENOENT`. The loader now keeps glibc's
   1664-byte surplus of static TLS, gives such a library a block there,
   and a thread copies the images of libraries opened since it last looked
   the first time it asks `__tls_get_addr` for one. With that the GPU
   process runs SwiftShader and the fallback is not taken; why the fallback
   meets `ENOENT` on Ferrix is not found.

**The window on ferrousli, 2026-09-26, and passing.** `cargo xtask
test-chrome-window --interpreter ferrousli --library ferrousli` stands
ferrousli in for glibc as `test-chrome` does, for the full browser: the
loader at `/lib64/ld-linux-x86-64.so.2`, where both Chrome's and its crash
handler's `PT_INTERP` look, `libc.so.6` in `/lib`, and `LD_LIBRARY_PATH`
given to what the compositor starts. The full browser loads 80 objects
where the headless shell loads 46 -- cairo, pango, CUPS, GnuTLS, Kerberos
and what they need -- and opens seven more later. Found by running it on
example's own kernel first, through a copy whose `PT_INTERP` names
ferrousli's loader, then passing on Ferrix at the first boot:

1. **The loader held 64 objects.** It refused the browser with "too many
   shared objects" before `main`. It holds 256; the scope, a few hundred
   bytes an object, is built where it is kept instead of on the loader's
   stack.
2. **`posix_fadvise64`.** Listing every glibc name the 79 libraries, the
   seven opened later and the two programs import, against what ferrousli's
   `libc.so.6` exports, left this one.
3. **NSS could not load its soft token**, and Chrome's `FATAL` in
   `nss_util.cc` ended the browser. NSS looks for `libsoftokn3.so` first in
   the directory `dladdr` names for `libnss3.so`, and the loader named every
   library by its `DT_NEEDED` name, which has no directory; then by name,
   and the loader's search skipped `LD_LIBRARY_PATH`, because it kept a
   pointer to the value in the environment block and Chrome had written
   its process title over that block. The loader copies the value now, as
   glibc does, and reports a library it searched for by the path it found,
   to `dladdr`, `dl_iterate_phdr`, `link_map` and `RTLD_DI_ORIGIN`.
   `tests/link.rs`'s check 85 requires both.

One trap in the host copy, which Ferrix does not have: the loader knows
itself by the file name of the program's `PT_INTERP`, so a copy that named
it `/tmp/cwf-ld.so` loaded glibc's `ld-linux-x86-64.so.2` beside it for the
libraries that need that name, and GnuTLS's finaliser faulted in glibc's
`__tls_get_addr` at exit. Named with glibc's file name, as on Ferrix, it
answers for itself.

Running Chrome on the host showed one thing that is the host's: with
`WAYLAND_DISPLAY` set, ANGLE wants `VK_KHR_wayland_surface`, which
SwiftShader offers only if it can open `libwayland-client.so.0`. glibc's
loader found the host's copy in `/lib/x86_64-linux-gnu`; ferrousli's, which
has no such default, found none, and the volume has none. On Ferrix the
variable is not set.

---

## 9. Chrome in a window, 2026-09-24

The customer asked to see it. `chrome-headless-shell` has no windowing, so
the volume carries the same version's full browser too, Chrome for Testing's
`chrome-linux64`, a 294 MB program. `tools/common/fetch/fetch-chrome.sh`'s library
check added the 32 Debian packages it needs beyond the headless one -- cairo,
pango, CUPS and what they load, GnuTLS and Kerberos among them -- in four
rounds; the volume is 1220 MiB.

Chrome's Ozone layer with `--ozone-platform=wayland` is a Wayland client
with its own libwayland, and draws through `wl_shm` with `--disable-gpu`. It
was run against the compositor on example's own kernel first, from the
volume's files, and drew its tab strip, toolbar and page there; the only
thing it needed was the crash handler's `PT_INTERP` pointed at the volume's
loader too, which on Ferrix `/lib64` does.

On Ferrix it drew its window, and ended its connection seven seconds later
on a protocol error: "no global 0 at version 3". The compositor made a
request's new object at its parent's version, which is Wayland's rule, and
refused it when that was above the version the object's own interface
declares -- which protocols do, pointer-gestures having its manager at 3 and
its swipe at 2. libwayland makes such an object all the same; the compositor
now makes it at its interface's version. The compositor also parsed
Hyprland's `env =` lines and gave them to nothing it started; it gives them
to every program now, which is how Chrome gets a home it can write.

`cargo xtask test-chrome-window` boots the compositor with Chrome showing a
page whose background is `#fc0`, and requires over a tenth of the screen in
that yellow with the compositor's background around the window; the first
run that passed had 532 386 such pixels on a 1024x768 screen, and its
screenshot is Chrome's own window chrome around the page.

`cargo xtask run-compositor --chrome` is the same on the desktop, with a
network: Chrome opens with the desktop, and SUPER+B opens another. `cargo
xtask remote-desktop --chrome` shows it from another machine.

**On the persistent btrfs root, 2026-09-26.** Until then `--chrome` booted a
tmpfs root, because on the desktop's btrfs root Chrome stopped after its
first Wayland requests or before them, with its profile in `/dev/shm` as
well as in `/tmp`. It was not btrfs's. When the kernel moves `/` onto the
btrfs volume it mounts a fresh devfs, `/proc`, `/sys` and a tmpfs `/tmp`
inside it (`src/kernel/src/fs/root_disk.rs`), but not the tmpfs `fs::init`
mounts over `/dev/shm` on the initramfs's root; so on a btrfs root
`/dev/shm` was devfs's bare directory, where nothing can be made. Chrome
ended at once, crashpad unable to make its database and `PathService` with
no user-data directory, and every POSIX shared memory object and named
semaphore on that desktop failed with it. The root's mounts include
`/dev/shm` now. `cargo xtask test-chrome-window --btrfs-root` boots on a
btrfs root made fresh for the run and requires the same page on the screen:
before the change it failed with Chrome's `Failed to get the path for 1001`,
after it 532 386 yellow pixels, as on tmpfs. `--chrome` no longer forces a
tmpfs root. The profile in `/tmp` is not tried again.

**How fast, 2026-09-26.** On the desktop, clicks took seconds to land and
some were lost. `cargo xtask bench-chrome` measures it. The command boots
Chrome on the compositor with a page that turns a box forever and has a
thousand lines to scroll. It leaves the page alone for ten seconds, turns
the wheel for ten and sweeps the pointer for ten. Beside Chrome, a busybox
script reads every process's `/proc/<pid>/stat`, which now reports real
processor time (utime was always 0 before). It also reads `/proc/stat` and
`/proc/meminfo`. The report gives each phase's processor time by process
name, how busy the machine was, and the compositor's frames.

The first run, under KVM with `--gl`:

| phase | Chrome's processor time | machine busy | frames a second | ms a frame |
|---|---|---|---|---|
| left alone | 443% | 96% | 1.5 | 25 |
| scrolled | 451% | 97% | 7.3 | 23 |
| pointed at | 394% | 94% | 1.7 | 13 |

It was the kernel, not Chrome. A sampling profile showed almost every
sample in ring 0: the timer interrupt recorded the address it interrupted.
The three causes and their fixes:

- **A futex wait woke every 5 ms to look again.** This was the wait queue's
  own safety net against a lost notify. A futex wake, a signal and a
  process ending all wake the waiter by name, so the net caught nothing.
  Chrome parks about 150 threads, and each woke 200 times a second. A
  futex wait now trusts its wakes, as `poll` does. On its own this took
  Chrome at idle from 443% to 191%, and the animation from 1.5 frames a
  second to 45.
- **Each wake-up read the HPET, and each HPET read exits to QEMU.** The
  kernel prefers an invariant TSC. `qemu64` does not advertise one, so
  under KVM the clock was the HPET. A KVM boot now asks for `+invtsc`, and
  the log says `clock TSC, calibrated against the HPET`.
- **`munmap` of a reservation walked every page of it.** Chrome's allocator
  hands back gigabytes of address space it barely touched. The unmap walked
  from the root once per page, found nothing, and stepped one page. It now
  steps over an absent descriptor's whole span. A gigabyte with one page at
  its end reads 5 118 descriptors instead of about a million.

With all three:

| phase | Chrome's processor time | machine busy | frames a second | ms a frame |
|---|---|---|---|---|
| left alone | 35% | 17% | 58.6 | 4.6 |
| scrolled | 84% | 31% | 48.0 | 6.2 |
| pointed at | 44% | 21% | 60.1 | 4.6 |

The screen refreshes at 60 Hz, so an animation at 58.6 frames a second is
all of them.

Two more followed the same night, both found the same way. The first was
a profile whose kernel samples were tagged with the process they ran
for, and shootdowns counted by the syscall in progress. The second was
the driver's own user samples.

- **Every write to a page after a `fork` was a TLB shootdown.**
  `AddressSpace::with_page` carries every copy to a program's memory. It
  faulted the page in first and looked second. In a copy-on-write region
  (every region of a process that has ever forked), a write fault takes
  the page down and puts it back, even when the page is already this
  process's alone and writable. Chrome's browser process forks. Every
  `clock_gettime` it made, forty thousand a second, wrote its timespec
  through a shootdown: thirty thousand shootdowns a second, most sent to
  another processor and waited for. `with_page` now looks first and
  faults only when it must. Chrome while scrolling went from 64% to 28%.
- **The GPU driver was billed for QEMU's work.** QEMU's virtio-gpu
  devices default `ioeventfd` off, unlike its other virtio devices. The
  driver's doorbell was then an exit that held the guest's processor
  while QEMU handled the queue, the frame's GL included. Nearly all the
  driver's samples fell on the instruction after the doorbell's store.
  xtask now gives every card `ioeventfd=on`. The driver went from 12–20%
  of a processor to 1%.

| phase | Chrome's processor time | machine busy | frames a second | ms a frame |
|---|---|---|---|---|
| left alone | 15% | 6% | 60.8 | 4.0 |
| scrolled | 29% | 10% | 60.8 | 5.7 |
| pointed at | 26% | 10% | 60.4 | 4.3 |

What was left then was Chrome's own work and its forty thousand
`clock_gettime` calls a second. **A vDSO, the same day**, answers them in the
program: glibc finds `__vdso_clock_gettime` through `AT_SYSINFO_EHDR`, reads
the TSC and scales it by the frequency on a data page the kernel keeps. The
calls went from about 45 000 a second to none -- all system calls together
from about 56 000 a second to 8 000 -- and the boot check holds a program's
call through the vDSO between two system calls, reading the TSC under KVM.

| phase | Chrome's processor time | machine busy | frames a second | ms a frame |
|---|---|---|---|---|
| left alone | 13% | 5% | 60.4 | 4.1 |
| scrolled | 27% | 10% | 60.3 | 4.8 |
| pointed at | 22% | 8% | 60.5 | 4.8 |

**The host's side.** A wake of a task on another processor used to
interrupt every other processor. On four, that was three interrupts to tell
one, and under KVM each is an exit and a host thread woken. The kick now
interrupts the one processor, and sends nothing while an earlier kick's
interrupt has yet to arrive. Over six runs of each, alternated, QEMU's own
processor time on the host fell from 80.9% of a host core to 65.0%, and
every phase still drew 60 frames a second.

A first measurement had held that back for a lost second of frames in one
phase in five. The bench was wrong, not the kick. The compositor reports
its frames at the first frame a second or more after its last report, about
every 1.015 s, and a phase is ten seconds and some 40 ms. So a phase holds
ten reports, or nine when it began just after one. Counted over ten
seconds, nine reports read as 545 frames where 605 were drawn, and main's
own runs did it as often. `bench-chrome` now gives a phase's reports beside
its frames, and its frames a second are frames per report. A real stall
still shows: the report that covers it counts fewer frames for its second.

**On Windows, 2026-09-29.** On `run-compositor --everything` Chrome
answered a click half a minute late, and the guest's `btop` showed its
one processor at 100%. Two causes. The QEMU xtask boots on Windows
(`docs/GPU.md` §3.12) aborts under WHPX on a machine that is not an x86
one, and `auto` probed WHPX with `-M none`, so the probe failed and the
desktop ran under TCG on four emulated processors. And WHPX, like KVM
before `+invtsc`, left the kernel on the HPET, each reading an exit that
QEMU's own x86 emulator decodes. The probe now starts a `q35`, and WHPX
is given `+invtsc` as KVM is. `cargo xtask bench-chrome --accel whpx`,
one processor:

| clock | machine busy | frames a second |
|---|---|---|
| HPET | 100% in every phase | 16–36 |
| TSC | 19–25% | 60 in every phase |

**Pages that took long to load, 2026-09-30.** With clicks answered,
pages on the same desktop still loaded slowly. Measured in the guest over
`--ssh`: name lookups, connects and TLS took about 0.1 s, and a ping to the
gateway 3 ms, but a download ran at 0.33 MB/s where the host fetched the
same file at 23 MB/s. Counters in xtask's gateway (`tools/common/xtask/src/gateway/tcp.rs`) showed why:
it kept a whole 64 KiB window in flight, more than the guest's network
driver has receive buffers posted for, so segments were dropped between
QEMU and the guest; each drop answered by three duplicate acknowledgments
sent the whole window again, which overran the buffers again, and 8 MB
went over the wire five times. The gateway now keeps eight segments in
flight, sends only the missing one on duplicate acknowledgments, and
retransmits on a 20 ms timer rather than 200 ms:

| gateway | 4 MB download in the guest | Wikipedia article, 261 KB |
|---|---|---|
| before | 0.33–0.41 MB/s | 0.44–0.66 s |
| after | 4.0–5.2 MB/s | 0.21 s |

Four downloads at once share about 4.3 MB/s. What is left is the guest's
receive path: a sweep of the in-flight cap found 8 fast and 16 already
losing most of what was sent, so the driver takes fewer frames at once than
its queue suggests, and raising that is the next step past these numbers.

**Brief slowdowns are the host's.** Some runs still dip for a second or
two, to 42–56 frames a second in a phase, and the dips come and go between
runs of the same kernel. To find out why, two probes ran side by side over
four runs. They were not landed.
- **In the guest:** the compositor logged every gap of 25 ms or more
  between its frames, with its own clock and where the frame's time went.
- **On the host:** a sampler read QEMU's per-thread `schedstat`, which
  gives each thread's time running and its time waiting for a processor,
  every 10 ms. It also read the host's `/proc/stat` and, every half second,
  its busiest processes.

Of the 13.4 s lost to gaps of 40 ms or more, 12.9 s fell while the host's
24 processors were at least 90% busy. In those stretches, QEMU's
virtual-processor threads spent 50–100% of their time runnable and waiting
in the host's run queue, and the compositor's flip took 25–390 ms instead
of 2. The flip hands the frame to QEMU's GPU and waits for it. The load was
other work on the same host: compilers on 11–16 processors, and other
virtual machines. Runs taken while the host was quiet had almost no dips.

**What is left inside the guest** is small: six gaps of 46–247 ms over four
runs, 0.54 s in all. In most of them the compositor was idle and Chrome sent
its next frame late; twice the compositor's own frame took 28–49 ms. So
read a dip in `bench-chrome` against the host's load before blaming a
change for it.

**A video with its sound, the same day.** The customer heard YouTube
stutter on `run-compositor --everything`: the sound broke up and the
picture jerked. `cargo xtask bench-chrome-video` measures it. It boots
`bench-chrome`'s desktop with Chrome playing a local video, looping:
720p30 VP9 with a continuous 440 Hz tone in Opus, as a video site sends
to a window that size. The sound goes through the virtio-snd card into
QEMU's `wav` backend. Each phase is reported as `bench-chrome` reports it,
and then:

- the frames the page showed and dropped each second, and how far its
  clock moved;
- the kernel's underrun lines;
- the frames the device took in each host second;
- the silences and jumps inside the tone.

`FERRIX_BENCH_VIDEO` names another video. The C library is named, never
defaulted: glibc, or ferrousli with `--interpreter` or `--library`.

The first runs found Chrome's processor time climbing phase by phase on
the same video (41%, 98%, 182%), a quarter of the video's frames dropped,
and silences of 21–512 ms in the tone. The timer sampler of the speed
work, applied again, showed the renderer spending twice as long in the
kernel as in its own code. One processor in five went on `_mm_pause`,
spinning for the futex table's lock in `futex::wait` and `futex::rouse`.
The table was one lock over one list of every waiter in the system. The
renderer made about 4,500 futex calls a second, and each one scanned the
browser's hundred and fifty parked threads with the lock held. Under KVM
a holder's virtual processor can also be descheduled by the host, and
every other caller then spins until it runs again.

The table is now 256 buckets with a lock each, as Linux's is
(`src/kernel/src/syscall/futex.rs`). A boot check requeues a waiter across two
buckets, with a negative control. Three runs of each, alternated one at a
time on glibc under KVM, the host at a load of 25–39 throughout (38.6,
28.3 and 39.5 at the before runs' starts, 37.1, 25.4 and 38.0 at the
after runs'):

| | before | after |
|---|---|---|
| Chrome's processor time a phase | 53–166%, mean 96%, climbing within a run | 40–88%, mean 62%, flat |
| the guest busy | 28% | 22% |
| video frames dropped | 980 of 3,395 (29%) | 636 of 3,429 (19%) |
| the futex lock in the sampler | about 20% of a processor | about 0.5% |
| underruns | 3 | 5 |
| silences in the tone, the loop's own left out | 5 | 11, 9 of them in one run whose load rose to 39 |

**What is left of the sound's gaps is the host.** At these loads they did
not move with the fix. A host sampler, run beside the bench, put QEMU's
four vCPU threads in the host's run queue for 14% of the time against 20%
running. The compositor's worst flips (up to 381 ms) matched the
sampler's own longest gaps (301 ms), as with `bench-chrome`'s dips above.
One run at load 25 had no underrun and no silence but the loop's.

**Real YouTube, over the network, the next night.** A probe kept outside
the tree (`~/ferrix-logs/chrome-perf/youtube/bench-page-probe.patch` on
example) lets the bench open any page with `--net`, taking a screen every
ten seconds. YouTube's embed refuses to play as a page of its own ("error
153"), and its watch page opens behind a consent dialog. So the probe opens
a page served from the host (`index.html` beside the patch, served by
`python3 -m http.server --bind 127.0.0.1`), which holds the embed in an
iframe; the guest reaches the host at 10.0.2.2. *Big Buck Bunny* played
there with its sound, through the ring-3 network driver (`playing.png`).
With the timer sampler applied, at a load of 34–39, the guest was idle
for two thirds of the samples. The renderer's futex calls spun for about
2% of a processor, on locks shared by the same words, as Linux's would.
The network driver hardly showed. The run had three underruns. Nothing
guest-side is left to account for them: they follow the host.

One trap: QEMU's `wav:PATH` backend, with its mixing engine off, runs at
its own default of 44100 Hz whatever the stream's rate. So a 48 kHz card
plays 8% slow into it, and Chrome's media clock with it. The bench uses
the engine at 48 kHz, as a desktop's sound server does.

Memory has not moved: 521 MiB is in use, and most of that is page cache
for Chrome's 294 MB program. `/proc/meminfo` counts the cache as used,
because it reports `Cached` as 0. A trial kernel, not landed, reported
each process's anonymous memory in proportion: every frame of an
anonymous object or shadow, divided by its holders. Chrome's processes
came to about 160 MiB between them. The browser holds 46, two renderers
36 and 27, and the rest 17 and less. Anonymous memory is what Chrome
itself asks for, and there is not much of it to take away. The rest of
the 521 is cached file pages and the kernel.

**What it looks like, the same day.** The customer's screenshot showed three
things. Chrome drew everything, its own tabs and toolbar included, in
DejaVu Sans Mono. It said it was Linux. And a bar under the toolbar said
Chrome for Testing is for automated testing only.

- **The font.** On the desktop `/etc/fonts` is foot's port: its
  `fonts.conf` includes `conf.d`, and the port makes none. fontconfig knew
  no generic family and no metric alias, so `sans`, `sans-serif` and
  `Arial` all fell to the one font it had, foot's monospace. The desktop
  now links `/etc/fonts/conf.d` to the volume's, Debian's. The faces are
  in the tree, in `assets/fonts/`, so no image depends on a font installed
  anywhere: Inter 4.1 as the sans-serif, and Liberation 2.1.5, the faces
  metric-compatible with Arial, Times New Roman and Courier New, which are
  Chrome's own defaults on Linux. Both are under the SIL Open Font
  License. xtask carries them to `/usr/share/ferrix/fonts` with
  `assets/fonts/fonts.conf`, which `FONTCONFIG_FILE` names. That file adds the
  directory, puts Inter first for `sans`, `sans-serif` and `system-ui`,
  asks for greyscale antialiasing with slight hinting, and then includes
  the system's configuration. A page's CSS `sans-serif` is not fontconfig's
  alias. It is a preference of Chrome's own, Arial on Linux, so a page
  gets Liberation Sans.
- **The name.** `--user-agent` gives `Mozilla/5.0 (X11; Ferrix; not Linux
  x86_64) ... Chrome/154.0.0.0`, and nothing else. It said `(Ferrix
  x86_64)` at first, and Google's search answered with its page for a
  browser it no longer supports, three times in three: a site that knows
  the platforms it serves does not know Ferrix. With the words `Linux
  x86_64` in it -- Ubuntu's Firefox said `X11; Ubuntu; Linux x86_64` for
  years -- the same search is answered as Chrome's own user agent is.
  Measured on the host:
  `navigator.platform` stays `Linux x86_64`, even with `uname` faked to say
  Ferrix. `navigator.userAgentData.platform` and the `Sec-CH-UA-Platform`
  header stay `Linux`. All three are constants in Chrome's build, and no
  switch reaches them. For a few hours an extension of the tree's own
  rewrote them in each page. The customer did not want an extension only
  to say Ferrix, and chose the flag alone over patching Google's binary
  or building Chromium. So Ferrix says Ferrix (`uname -s`), and Chrome's
  user agent does. The platform in `navigator` and the client hints says
  Linux until Chrome is built from source. With a user agent of its own,
  Chrome also leaves the `architecture` client hint empty.
- **The bar.** `--disable-infobars` removes it.
  `test-chrome-window` now counts 623 621 yellow pixels where it counted
  532 386, because the page has the bar's height.

The user agent has spaces, and the compositor split an `exec` line at every
space. It now splits the line as `sh -c` would for quoting alone
(`src/user/system/linux/compositor/hyprix/src/command.rs`), which is what a Hyprland
configuration assumes, since Hyprland hands the line to the shell.

---

## 10. On the STM32MP157D-DK1, sized 2026-09-24

The customer asked what Chrome on the board would take. The board has two
800 MHz Cortex-A7 cores, 512 MiB of RAM, a Vivante GC400 and an SD card, and
Ferrix already runs its HDMI Wayland desktop with a USB keyboard and mouse.
Everything the x86-64 run needed of the kernel -- `execve` from the file,
`/proc/self/exe`, `madvise`, `timerfd`, `signalfd`, `creat`,
`clock_getres` -- is on ARMv7-A too, and Debian's armhf glibc runs there.
What is not, in points:

| what | points |
|---|---|
| An ARM browser: Chrome for Testing is linux64 only, so Debian 13's Chromium 150 for armhf (199 MB installed), on a volume `fetch-chrome.sh` makes the same way | 3 |
| The SD card at run time: U-Boot loads everything into RAM as the initramfs, and there is no SDMMC driver, so a 1 GB volume has nowhere to be; a ring-3 driver behind the block ring, then btrfs from a partition | 13 |
| Memory: 512 MiB, no swap, and the page cache never evicts a file's pages, so every page of the program Chrome touches stays; eviction under pressure, with `--single-process` and one page at a time | 8–13 |
| V8's JIT flushes the instruction cache with ARM's `cacheflush`, which `src/lib/proto/linux-abi` numbers and the kernel does not answer; or `--js-flags=--jitless` | 1–3 |
| What running it finds: on x86-64 that was six things in a day | 13 or more |
| The board's Ethernet, a DWMAC with no driver, if pages are to come from the network | 8 |

About 45 to 55 points to a slow but real Chrome window on the board, most of
it the SD driver and the memory. The drawing is in software on two A7 cores:
seconds a page. The GC400 work under way is GLES2, below what Chrome's GPU
path wants, so it does not help here soon. Memory is the risk: 512 MiB may be
too little whatever is built.

