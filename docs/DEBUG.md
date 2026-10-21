# Debugging a running Ferrix

On 2026-10-21 a DXVK thread of Endfield froze on the N150 box's Intel iGPU
(`docs/N150.md`), and nothing on the box could say what it was waiting for.
Ferrix had no `ptrace`, so no gdb, gdbserver or strace for a process. It had
no `/proc/<pid>/{syscall,wchan,stack,environ}`, no perf, and no kernel
debugger on bare metal. The only debugger was QEMU's gdb stub
(`cargo xtask run --gdb`), which no real machine has.

The customer asked for remote debugging in four slices, the most useful
first, each landed on its own:

| Slice | What | State |
|---|---|---|
| S1 | `/proc/<pid>/task/<tid>/syscall`, `wchan`, `stack`, and `/proc/<pid>/environ` | this landing |
| S2 | A Linux-compatible `ptrace` subset, enough for gdbserver and strace on x86-64 | design draft (§4), review pending |
| S3 | gdbserver and strace on the box over the network; attaching to a Wine process with symbols | after S2; plan in §5 |
| S4 | A kernel gdb stub over the network or serial for bare metal, behind a boot option, off in certified profiles | design sketch (§6) |

---

## 1. S1: what a thread is doing, read from outside

Four files, each in Linux's format, so `cat`, `ps -o wchan`, and the tools
that read Linux's files read them unchanged:

| File | Says | Who may open it |
|---|---|---|
| `/proc/<pid>/task/<tid>/syscall`, and `/proc/<pid>/syscall` for the first thread | `running`; the call the thread is blocked in, as `nr arg1 ... arg6 sp pc` (the number in decimal, the rest in `0x` hex, as Linux prints them); or `-1 sp pc` for a thread blocked outside any call (a page fault). x86-64 only for now: `EOPNOTSUPP` on AArch64 and ARMv7-A (§1.1) | a reader `credentials::may_access` allows, Linux's `ptrace_may_access`, as for `maps`; otherwise `EACCES` |
| `.../wchan` | what the thread waits for, as one token: the call's name, and for a call on a descriptor that descriptor's `/proc/<pid>/fd` text after a colon (`read:pipe:[42]`, `ioctl:/dev/dri/renderD128`, `futex`, `channel_write_read`, `fault`); `0` when it runs, waits in no call, or the reader may not look | anyone (mode 0444), but `0` unless `may_access` allows, as Linux's `proc_pid_wchan` does |
| `.../stack` | the thread's kernel call chain while it waits, innermost first, one `[<0>] 0x<address>` line a frame, each address the one the image was **linked** at, with the KASLR slide taken off | root in the first user namespace (Linux's `CAP_SYS_ADMIN` test in `proc_pid_stack`), and then `may_access`; mode 0400 |
| `/proc/<pid>/environ` | the environment block in the process's own memory: the bytes from the first environment string to the end of the last, read as Linux reads `env_start..env_end` | `may_access`, as Linux's `PTRACE_MODE_READ` for `environ` (p2-139) |

Each file is rendered when it is opened, like the rest of `/proc`, and a
refusal comes at the open: a reader who may not look gets `EACCES` from
`open`, where Linux returns `EPERM` from `read` for `syscall` and `stack`.
No program on the box tells the two apart. The descriptor `wchan` names is
the one the call's argument names now, so a descriptor closed and reused
since the call began names its successor, as Linux's would.

### 1.1 Reading a task that is not running

This is the part that matters: how the kernel learns what a blocked thread
is doing. It adds nothing to any path a program takes. There is no new store
on a system call's entry or exit, on a block, a wake or a switch, and nothing
on the native channel round trip (§1.4). Everything the view needs is
already in memory.

* **The call (x86-64).** A 64-bit program's `SYSCALL` saves its registers
  as `ferrix_syscall_stub`'s `SyscallFrame` at `gs:8`, which
  `set_entry_stack` points at the task's kernel stack top. A trap from ring
  3, a page fault among them, and a 32-bit program's `int $0x80` push a
  `TrapFrame` from `RSP0`, the same top. A blocked task's frame stays there
  until it returns. `Task::in_call` (stored by `carry_in_call` at every
  switch) says whether the task is inside a call. The process's program ABI
  (kept at `execve`) says which entry that call came through, so the rule is:
  in a call with the 64-bit ABI, the 128-byte `SyscallFrame`; otherwise the
  176-byte `TrapFrame`. The view never guesses from the words. Field
  positions are `offset_of!` of the frame structs the assembly is already
  asserted against (`arch/x86_64/inspect.rs`).
* **Not yet on AArch64 and ARMv7-A.** Their vectors push the frame on
  `SP_EL1`, or the SVC stack pointer, as the task's last return to user mode
  left it. `ferrix_enter_user` and `ferrix_resume_user` leave that wherever
  their caller's stack stood, so it is no fixed place. On 2026-10-21 the
  debug step read `syscall_2735960` and `restart_syscall` from a frame
  assumed to be at the top. So `syscall` answers `EOPNOTSUPP` there and
  `wchan` reads `0`, rather than print a wrong call. The fix is in the cold
  user-entry paths (BACKLOG p2-140).
* **The chain (all three).** A task that is not running was switched away
  from by `ferrix_switch`, which leaves its stack pointer in
  `Task::stack_pointer`. The frame pointer and the return address sit at a
  fixed slot of that switch frame on each architecture (`rbp`, `x29`,
  `r11`). The kernel is built with frame pointers (`backtrace.rs`), so the
  view follows the chain with tighter bounds than the panic report: every
  record inside the task's own stack, each above the last and aligned, and
  every return address inside the kernel image, at most
  `backtrace::MAX_FRAMES`. A word that fails a test ends the chain and is
  never printed. Each address that passes has the image's slide taken off.
* **Sampled under the queue's lock.** The saved stack pointer, the state,
  `in_call` and the switch count are read holding the lock of the run queue
  that owns the task, with interrupts masked. That is how `switch_chosen`
  writes them, and it holds that lock until the incoming context's
  `finish_switch`. So a task found blocked under that lock, and not the
  queue's current, has finished its switch away, and its saved stack pointer
  is the one it will resume from. There is no window between the switch's
  bookkeeping and its save. Nothing else is done under that lock (bound:
  `MEMORY-AND-TIMING.md` §2.2t).
* **Every word read without a fault.** The task may wake, run, exit and have
  its stack freed by the reaper while the view reads it, and another
  processor may be writing the words. So each word goes through
  `mm::read_kernel_word`. That function translates the address through the
  kernel's page tables while holding their lock (`TABLES`), which an unmap
  that frees an emptied table lets go before the free and its shootdown. It
  reads the word through the direct map's alias of the frame found, and only
  if `direct_map_ram` says that frame is RAM, which the direct map holds for
  the kernel's whole life. A stack freed after the lock is let go is read as
  whatever its frame now holds: wrong, never a fault. A translation to a
  device or to nothing answers nothing. Every read is one aligned word,
  single-copy atomic on all three architectures, taken as an untrusted
  integer and never dereferenced. That is the one new `unsafe` site.
* **The words are advisory.** The switch count is read again after the
  words, behind an acquire fence, and a task that was switched to in between
  is reported as running. The switch's own count is a relaxed store, though,
  so on Arm a read that races a wake can still mix old and new words.
  Nothing in the kernel acts on a view: it is printed.
* **Nothing about the kernel's layout is printed.** The call's fields are
  the program's own registers. The chain is return addresses that passed
  `in_image`, with the image's slide taken off, so each is the address in
  the ELF and reveals no slide. No stack pointer, frame address or anything
  else derived from the direct map or the vmap arena is printed
  (`docs/certification/SPECULATION.md` §6.1).

What the view cannot show: a runnable thread that is not running
(preempted) reads as `running`. A 64-bit program's `int $0x80` is read
through the 64-bit frame and comes out wrong. The fault case (`-1 sp pc`)
and a 32-bit program's frame are read by the same rule but are not claimed,
because no check drives them. A kernel task, with no user half, has no
directory in `/proc`.

### 1.2 Where it lives

* `src/kernel/src/sched/inspect.rs` (**core**): the view, `call_of` and
  `chain_of`, sampled by `sample`. It contains no `unsafe` of its own
  besides the queue-locked `saved_stack_pointer` read that `switch_chosen`
  already makes. `sched/inspect_check.rs` is its boot check.
* `src/kernel/src/mm.rs` (core): `read_kernel_word`, the one new `unsafe`.
* `src/kernel/src/arch/inspect.rs` (core, names no architecture): the
  layout types. `src/kernel/src/arch/{x86_64,aarch64,armv7a}/inspect.rs`:
  each architecture's frames, re-exported through the facade.
* `src/kernel/src/sched/task.rs` (core): `Task::in_call()`, a read-only
  accessor. `backtrace.rs` (core): `in_image`, `slide` and `image_len`.
* `src/kernel/src/fs/procfs.rs` and `fs/procfs/debug.rs` (load): the four
  files, who may open them (`guard_of`, `may_open`), and `wchan`'s naming.
  The naming needs the descriptor table and the call names
  (`ferrix_linux_abi::nr::Syscall`, `ferrix_native_abi::nr::NativeCall`),
  which the item does not.
* `src/kernel/src/syscall/exec.rs` and `syscall/process.rs` (load):
  `execve` keeps `env_start` and `env_end`, which `ferrix_ustack::build`
  already computes, and the program's ABI, beside the argument vector.
  `fork` copies them with the rest of the identity. A process that no
  `execve` started (a native one, from `launch`) keeps an empty range, so
  its `environ` is empty and never reads another program's memory. Every
  thread shares its process's range. `environ` copies the range a page at a
  time with `uaccess::copy_from_user` from the process's own space, at most
  2 MiB. It faults pages in there as the process's own faults would and
  holds no procfs lock. A page it cannot read ends the file there, as
  Linux's `access_remote_vm` stops short.
* `cargo xtask symbolize --arch <arch> [--release] < stack.txt` names each
  `stack` line's address from the kernel ELF this tree built
  (`tools/common/xtask/src/symbolize.rs`, which the panic report already
  uses).

### 1.3 Requirements and checks

`H.SCHED.15`: a task that is not running shall be reportable from outside
it, from what its entry and switch already saved, without the report
faulting, changing the task, or adding work to any path the task takes.

| Id | What | Check | Negative control |
|---|---|---|---|
| L.sched.86 | On x86-64, a thread blocked in a call through the 64-bit entry is reported with its number, six arguments, stack pointer and instruction pointer from the frame at its stack top; the reader's own thread as running | `test-procfs`'s `debug` step on x86_64: a thread blocked in `read` of a pipe with a known descriptor, buffer and length; the reader's own file is `running` | the frame's first argument read from the second's place: `procfs: FAILED debug: syscall` |
| L.sched.87 | A blocked task's kernel chain from its switch frame, each record inside its stack, each return address inside the image, at link addresses | `sched::inspect_check` at boot, every architecture: a kernel task records its own chain with `backtrace::walk` and blocks; the view's chain of it must end in the recorded one, every address inside the image as linked | the frame pointer slot moved by a word; the slide not taken off (the boot prints the slide, which is not zero under KASLR) |
| L.sched.88 | A word of a freed stack answers nothing, without a fault | `inspect_check`: a stack from `vmap`, read while mapped, then freed and read again | the read made through the address itself: a kernel page fault |
| L.sched.89 | A word whose translation is no RAM answers nothing, the read not made | `inspect_check`: a `vmap` device window onto 512 GiB, where no boot machine has RAM | the RAM test taken out: a kernel page fault on x86_64, or the check's own sentence, "a word of a device window was read", where the direct map reaches that address |

The access rules are the load ring's and are proved where the others are:

* `fs::procaccess_check` (`docs/NAMESPACES.md` M8): `environ` and `syscall`
  join the entries a reader who may not look is refused and one who may is
  answered; `stack` is refused to uid 1000 on its own process and read by
  root.
* `test-procfs`'s `debug` step, with real blocked threads: a uid 1000 child
  of the root test is refused `syscall`, `stack` and `environ`, and reads
  `wchan` as `0`. On the Arm architectures the step checks `stack` and the
  refusals, and that `syscall` is `EOPNOTSUPP` and `wchan` `0`.
* `environ` is read back as the program's own environment block, and a
  byte changed in place there reads back changed.

Each rule has its own control, through `gate.sh control`: `syscall`,
`environ` and `stack` with the rule taken out, which `procaccess` must fire
on; and `wchan` printing its token to a refused reader, which the debug
step's child must fire on.

### 1.4 The IPC fast path

Nothing in S1 runs on it or on any system call's way in or out. The frame
the view reads is the one the stub already pushes, `in_call` is the flag
`carry_in_call` already stores, and the switch count is the one
`switch_chosen` already keeps. `git diff` of `arch/*/syscall.rs`,
`arch/*/trap.rs`, `arch/*/switch.rs`, `sched/direct.rs`, `object/channel*`,
and of every function in `sched/mod.rs` and `sched/task.rs` that a call or
switch runs, is empty. The only additions to `sched/mod.rs` are the two
module lines at its end. So S1 claims the round trip unchanged by
construction and measures nothing. S2 measures it before and after (§4).

---

## 2. Using it on the box

```
# every thread of a process: which call, which wait
for t in /proc/$PID/task/*; do
  echo "$(basename $t) $(cat $t/comm) $(cat $t/wchan) | $(cat $t/syscall)"
done
# a thread's kernel chain, as root
cat /proc/$PID/task/$TID/stack > stack.txt
#   then, on the machine that built the image:
cargo xtask symbolize --arch x86_64 --release < stack.txt
# what a process was started with
tr '\0' '\n' < /proc/$PID/environ
```

---

## 3. Where it stands

* 2026-10-21: S1 designed (§1). Ids reserved: L.sched.86-89 (49a24aee1)
  and H.SCHED.15 (a2d87fe62). Consultant's design review: **OK IF (1)-(10)**,
  ledger 2831. Condition (1) is met by sampling under the queue's lock
  rather than an atomic `stack_pointer`, so the switch path is unchanged.
  Condition (2) is met by `mm::read_kernel_word` under `TABLES`. Conditions
  (3)-(9) are met in §1.1-§1.3 and `MEMORY-AND-TIMING.md` §2.2t. One change
  from the reviewed design: no call view on AArch64 and ARMv7-A (§1.1,
  p2-140), so L.sched.86 is x86-64's.
* Code review: **OK IF R1-R5**, ledger 2909, the departures (A)-(D) accepted:
  the queue-locked sample, `read_kernel_word` under `TABLES`, no call view
  on Arm, and the environ test against the program's own block. The call
  view's coverage is owed (p2-141). The open-then-render window of the
  private entries is named, not fixed (p2-142).

---

## 4. S2: `ptrace` (design draft, not yet reviewed)

This section is the design note S2's code waits on. The consultant reviews it
before any code is written, since it is the larger change to the certified
item.

### 4.1 The subset

Enough for Debian's gdbserver and strace on x86-64, as they call it:

| Request | Meaning here |
|---|---|
| `PTRACE_ATTACH`, `PTRACE_SEIZE` (+ `O_TRACESYSGOOD`, `O_TRACECLONE`, `O_TRACEFORK`, `O_TRACEVFORK`, `O_TRACEEXEC`, `O_EXITKILL`), `PTRACE_INTERRUPT`, `PTRACE_DETACH`, `PTRACE_TRACEME` | a thread gets a tracer; `ATTACH` also sends `SIGSTOP` |
| `PTRACE_GETREGS`, `SETREGS`, `GETFPREGS`, `SETFPREGS`, `GETREGSET`/`SETREGSET` (`NT_PRSTATUS`, `NT_PRFPREG`, `NT_X86_XSTATE`), `PEEKUSER`/`POKEUSER` for the debug registers' offsets | read and write a **stopped** tracee's saved frame (§1.1's layouts) and its `UserState` (the `XSAVE` area) |
| `PTRACE_PEEKDATA`/`POKEDATA`, `/proc/<pid>/mem` (read and write), `process_vm_readv`/`writev` | `uaccess::copy_{from,to}_user` against the tracee's space; a write to a read-only text page is a forced copy-on-write, as Linux's `FOLL_FORCE` |
| `PTRACE_CONT`, `SYSCALL`, `SINGLESTEP`, `LISTEN`, `KILL` | resume with a signal, stopping at the next call's entry and exit, after one instruction (`RFLAGS.TF`, returned through `IRET`, which the `SYSCALL` path's comment already names), or not at all |
| `PTRACE_GETSIGINFO`, `SETSIGINFO`, `GETEVENTMSG`, `GET_SYSCALL_INFO` | what the stop was |
| `int3` | already `SIGTRAP` (`arch/x86_64/trap.rs`, `#BP`); a traced thread stops instead of dying, and the debugger rewinds `rip` |

`wait4`/`waitid` report tracee stops to the tracer as Linux does
(`WIFSTOPPED`, `status >> 8 == SIGTRAP | 0x80` for a call stop,
`PTRACE_EVENT_*` in bits 16-23). They are added beside the job-control
stops `wait4` already reports with `WUNTRACED`. A tracee is its tracer's
child for waiting only. `getppid` and `/proc` keep the real parent.

### 4.2 Who may trace

* **`credentials::may_access` with `real` false is `PTRACE_MODE_ATTACH`**.
  It is the same function `/proc` asks, already proved by
  `fs::procaccess_check`. A process that is not dumpable is traceable only
  by root in the first namespace.
* **A Yama-like scope, `/proc/sys/kernel/yama/ptrace_scope`**: 1 by default
  (a tracer may attach only to its descendants, or to a process that
  `PR_SET_PTRACER` named), 2 (root only), 3 (none, for good). gdb attaching
  by pid on the box then needs root or scope 0, as on Debian.
* **Never across jobs without rights.** A tracer must be in the tracee's
  `Job` or an ancestor of it. A process holding only native handles (a
  ring-3 driver, `devmgr`) is never a tracee: the native ABI has no ptrace,
  and its tasks are refused `EPERM`. The certified profile can turn the
  whole facility off at build time (`build-flags.toml`, `debug=off`), and
  the flag is printed on the `build` boot line.

### 4.3 Where a stop happens

A stop is a block on the tracee's own path back to user mode, on the
`sched::work` "way back" list that signals already use. The tracer reads
and writes the tracee's registers only while it is stopped there. The
frame the stop reads and writes is the one §1.1 reads; the kernel does not
copy registers anywhere new. `SETREGS` validates selectors and flags as
`rt_sigreturn` does. `SINGLESTEP` returns through `IRET` with `TF` set; the
`#DB` that follows is a stop.

### 4.4 Staying off the IPC fast path

The untraced path must not get one instruction more. The seccomp design
already pays for this: each thread has a `watched` mask that the `SYSCALL`
entry loads anyway, and the fast path asks `seccomp::quiet()` first. That
function is a single load of a global count of watched threads, and while
the count is zero nothing else is read. A traced thread raises a new bit,
`WATCH_TRACE`, in the same mask and is counted in the same global. So:

* a machine where nothing is traced runs exactly today's instructions on
  `channel_write_read` and on every Linux call;
* while anything is traced, the fast path declines for the traced threads,
  and for others pays the per-thread `watched` load it already pays while
  any seccomp filter exists;
* call stops are taken in the general entry, after `seccomp::check`, where
  Linux takes them.

The round trip is measured before and after with
`.claude/skills/optimize-ipc-round-trip`'s protocol (x86_64 under KVM, on
an idle host, the skill's command and its number of runs), and the S2
landing quotes both figures. An `objdump` of the fast path's entry,
identical before and after, is the second evidence.

### 4.5 Requirements and checks (to reserve)

An `H` parent and about eight `L` ids:

* attach refused by `may_access` and by the scope;
* refused across jobs and to native processes;
* a register round trip;
* a memory round trip through `PEEK`/`POKE` and `/proc/<pid>/mem`;
* a call stop at entry and exit;
* one instruction per `SINGLESTEP`;
* `int3` reported as a stop;
* a tracee's exit and exec reported; `DETACH` resumes it.

A user test program, `src/tests/ptrace`, drives them under
`cargo xtask test-ptrace`. Each check has a negative control, through
`gate.sh control`.

## 5. S3: on the box (after S2)

* **gdbserver and strace from Debian**, as the box's other tools are
  (`docs/N150.md`): the `gdbserver` and `strace` packages for amd64, put
  under `/data/debug/` on the box's btrfs root, run with their own
  `ld.so` the way Debian's busybox runs in `test-shell`.
* **Reachable over the network**: `gdbserver --attach 0.0.0.0:2345 <pid>`
  on the box, `target extended-remote <box>:2345` from gdb on the host,
  through the WireGuard link the box already uses. Root on the box, since
  scope 1 refuses attaching to a non-descendant.
* **A Wine process with symbols**: attach to the `wine64-preloader` thread
  that §1's `wchan` and `syscall` named. Then `set sysroot` to a copy of
  the box's Wine tree on the host, `info sharedlibrary`, and
  `add-symbol-file` for DXVK's `.dll.so` files at the bases
  `/proc/<pid>/maps` lists. The recipe gets a test on QEMU first: `test-wine`
  with gdbserver attached to the preloader.

## 6. S4: a kernel gdb stub (design only)

* **What**: gdb's remote serial protocol (`?`, `g`/`G`, `m`/`M`, `c`, `s`,
  `Z0`/`z0` software breakpoints, `Z1` hardware ones through `DR0`-`DR3`,
  `qSupported`, `T` stop replies), served by the kernel itself.
* **Transport**: the serial port the kernel already writes, polled with
  interrupts masked on every processor once a stop is taken. It runs over
  the network only through a dedicated ring-3 relay, never through the
  kernel's own network stack, which a stopped kernel cannot run.
* **When**: only with `ferrix.kgdb=serial` on the command line, and only in
  a kernel built with `debug=kgdb` (`build-flags.toml`). The certified
  profiles (`present`) never build it. The flag shows on the `build` boot
  line, and `check-item-boundary.py` refuses the flag in the reference
  configuration. A stop holds every other processor in an NMI-driven
  rendezvous, and `#DB`/`#BP` taken in ring 0 enter the stub. It is a
  separate design review and its own slice.
