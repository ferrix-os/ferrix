# Stage 7 — The Linux syscall ABI ✅

The syscall entry path on every architecture, the dispatch table, and the core
of the surface: memory (`mmap`, `mprotect`, `brk`), files, process
(`clone`, `execve`, `wait4`, `exit_group`), threads and `futex`, signals with
`sigaltstack` and `rt_sigreturn`, time, and identity.

**Exit:** a static musl `busybox sh` starts, runs a script, and exits — the
first time somebody else's binary runs on Ferrix.

**The image's shell is zinc since 2026-09-18** (`docs/UUTILS.md` S4), and
`cargo xtask test-shell` runs zinc when no `--init` names another. That does
not retire this criterion, and it is why `--init` was kept rather than
removed: the point of the exit was *somebody else's* binary, and a shell
written against this kernel proves less about the ABI than one that has never
heard of Ferrix. Both are run. The busybox row below is the criterion; the
zinc row is the userland.

**Exit criterion met on all three architectures, with the script given to
`sh -c`.** `cargo xtask test-shell` builds the kernel with a static busybox
and a script, boots it, and requires the script's lines in order and its exit
status. Run against Alpine's `busybox-static` 1.37.0 — built by people who have
never heard of Ferrix — for x86-64, AArch64 and ARMv7-A:

    cargo xtask test-shell --arch all --init PATH/{arch}/busybox

      init     857 KiB program built in, starting `sh -c` with a built-in script
    script: started
    script: the sum is 15
    script: hello, ferrix
    script: test agrees
    script: case matched
    script: 3 positional parameters
      init     the shell exited with 7

The status is 7 rather than 0 so that a shell which died and reported success
cannot pass, and the lines are looked for after the boot marker so that the
kernel's own output cannot satisfy them.

Three decisions sit inside that, and each is a reading someone could dispute:

* **"Runs a script" is read as `-c`.** The script travels in `argv`, which
  needs no filesystem. A script *file* needs `openat`, and that is stage 8's —
  making this stage's exit wait on it would have put the first foreign binary
  behind a filesystem it does not otherwise need.
* **The script is builtins only.** Variables, arithmetic, a loop, a function,
  `test`, `case`, positional parameters and the exit status. Nothing forks,
  because `clone`, `execve` and `wait4` did not exist at the exit, and an
  external command would have measured their absence rather than the ABI.
* **The binary is not in the repository.** Which static busybox to trust is a
  decision for whoever runs the test, and a kernel that embedded a host's
  binary silently would stop building byte for byte the same. `--init` names
  it; without a script the same kernel starts `sh -i` and hands a person a
  prompt, which is how glibc busybox was first run by hand.

`test-shell` is not part of `cargo xtask check` or the boot test, because it
needs a binary the repository does not carry. The boot test covers every
handler below directly.

**Done:**

* **The numbers.** `src/lib/proto/linux-abi` carries all three tables. ARMv7-A's EABI
  table is not the 64-bit calls renumbered: a 32-bit register cannot carry a
  file offset, a file size or a post-2038 `time_t`, so sixteen calls exist twice
  and the wide form is a *different call with a different signature* — `mmap2`
  counts its offset in pages, `_llseek` returns through a pointer. Two more,
  `set_tls` and `cacheflush`, live at `__ARM_NR_BASE` and have no 64-bit
  counterpart. The boot test asserts which table this build uses by its content:
  each architecture reports its own number for `getpid`, 39, 172 and 20, which
  is the one fact a host test cannot establish.
* **The startup stack.** `src/lib/kernel/ustack` writes `argc`, `argv`, `envp` and the
  auxiliary vector at both pointer widths, and reads them back; its fuzz target
  found that a string containing a NUL built a well-formed image which read back
  as a different, shorter string. `execve` puts `AT_HWCAP` and `AT_HWCAP2` in it,
  from the core's own identification registers on both Arm architectures:
  musl's ARMv7-A `setjmp` saves `d8`-`d15` only when told there is a VFP. The
  bit values are checked against Linux's uapi headers and the fields are read
  as Linux reads them, signed where Linux reads them signed; `DCPOP` is not
  reported, because `DC CVAP` traps from EL0 without `SCTLR_EL1.UCI`.
* **Dispatch.** `src/kernel/src/syscall/`: `SyscallArgs`, `Outcome` and `dispatch`,
  reached through `arch::decode_syscall`, and total. The boot test puts every
  number in `0..=600` except `exit`, `exit_group`, `pause` and `alarm` through
  `dispatch`, with poisoned argument registers, from a task of a check process,
  so each call reaches its handler rather than a missing-process `ESRCH`. It
  fails if a call asks to enter user mode, ends or blocks that process, or
  leaves a frame behind.
* **The copy layer and the loader.** `copy_from_user` and `copy_to_user` resolve
  through the `AddressSpace` and its fault path rather than dereferencing, and
  refuse a kernel address before any length arithmetic. The ELF loader maps
  `PT_LOAD` segments with their own permissions and copies them in.
* **The calls a static binary makes.** Memory: `mmap`, `mmap2`, `munmap`,
  `mprotect`, `brk`. Threads: `set_tid_address`. Files: `read` on the console
  and `write`/`writev` to it, with iovecs read as native words. Time:
  `clock_gettime`, `clock_gettime64`, `gettimeofday`, `getrandom`. Identity:
  the credential calls, `getpid`, `gettid`, `getppid`, `uname`, `sched_yield`.
  Signals: `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`. Answered in each
  architecture's trap path, because they are facts about the processor:
  `exit_group`, `arch_prctl(ARCH_SET_FS)` and `set_tls`.

**What running foreign binaries found**, none of which a hand-written test
program would have:

* glibc spun forever on `clock_gettime(CLOCK_MONOTONIC)` returning `ENOSYS`,
  with no output at all — there is no vDSO, so it makes the real call.
* `brk(0)` answered the top of the user half, because the heap was placed above
  the highest mapping and the highest mapping is the stack. The first `mmap`
  then landed in the page deliberately left unmapped above the stack.
* `uname` reported `sysname` as `Linux`, because the programs that ask are
  choosing a code path. On 2026-09-13 the project's owner chose `Ferrix`
  instead: `Ferrix ferrix 6.1.0-ferrix`, with the release still a Linux
  version. A build that maps `uname -s` to a target is told which to use.
* ARMv7-A entered a Thumb-2 program in ARM state. An odd entry point is Thumb
  by the interworking convention, and Alpine's busybox enters at `0x1d1f9`.
* Neither Arm kernel let user mode use the FPU. ARMv7-A's busybox is hard-float,
  and AArch64's ran only because EDK2 happened to leave `CPACR_EL1.FPEN` open.
* busybox's `printf` asks `fcntl(1, F_GETFL)` before writing and prints nothing
  when it fails — confirmed by injecting `ENOSYS` into exactly that call on the
  host. `fcntl` belongs to stage 8's descriptor table, so `printf` joined the
  test script only with stage 8, after the transcript above.

**Signals were recorded, not delivered, at the exit.** The dispositions, the blocked mask and
the alternate stack answer consistently — the old action a program reads back is
the one it set, at its own architecture's layout, which is three native words
and an 8-byte mask. Nothing in a clean run raises a signal. Delivery is the
first item below.

**Programs are scheduled tasks.** Each program runs as a task of its own that
carries its process, rather than as a guest of the boot task with interrupts
masked. The boot test shows two sharing one processor and a third ended from
outside:

      procs    two programs took turns on one processor, switched to 45 and 49 times
      kill     a spinning program was ended from outside and reported 137

The check program spins in user mode, so it is preempted there only if
interrupts are open; masking them makes the first program run to the end the
first time it is switched to, and the check fails naming that rather than
merely running slower. `process::load` builds a process and `process::start`
runs it, so a handle can be put between the two; `process::kill` ends one from
outside; ending is a level (`Process::is_terminated`) and a wake-up, and
whichever of `exit_group` and `kill` arrives first sets the status. Three
things a program owns stopped being the processor's: its kernel entry stack,
its thread pointer, and its floating-point and SIMD registers, which the
scheduler now saves and loads on every switch between user tasks — eagerly,
next to the address space. And one thing that had been hiding: the `SYSCALL`
MSRs were programmed only on a processor that had started a program, which is
harmless when programs never move and a `#UD` when they do.

**A program makes programs.** `fork`, `vfork` and `clone` without new threads,
`execve` with one `#!` level, `wait4` and `waitid`, and the process-group and
session calls work on all three architectures, with two programs of their own
in the boot test:

      fork     a program forked, waited for its child, and exited with 24
      exits    4 programs that forked and exited gave every frame back once reaped, in window 1
      execve   a program became another and exited with 42; with the file gone it got errno 2

A child resumes from a copy of its parent's saved registers (`arch::UserRegs`,
entered by `arch::resume_user`) in a copy-on-write copy of its memory.
`execve` refuses everything it can before its point of no return, then empties
the process's own address space and loads into it, so the process keeps its
identity; a failure after that ends it with `SIGSEGV`'s status, as on Linux.
`vfork` copies rather than lends memory, and the parent still sleeps until the
child execs or ends. A new thread was `ENOSYS` at the exit; threads came
later (*Threads*, below), and only memory shared between processes without
`CLONE_VFORK` is still `ENOSYS`.

Every general register is cleared on entry to user mode, with one exception a
native process needs: `Startup.argument` arrives in the first argument
register -- RDI, x0 or r0 -- which is how stage 9's `process_start` hands a
driver its bootstrap handle. A Linux program's is zero. A starter claims the
start before it puts anything into the process (`process::claim_start`), so two
starts cannot both move a handle in or overwrite each other's argument; a
dropped claim gives the start back, and a process that has already ended cannot
be claimed. `exec::load_native` loads an image with nothing on its stack for
such a start. The boot test starts a two-instruction program that exits with
that register, through a claim:

      argument a program started with an argument found it on entry and exited with 57

Until 0510a8a every program leaked its task, its process and its address
space: `task_start` held the task's own reference across an entry that never
returns, and none of the checks that run programs counted frames, because an
unreaped task looks like a leak. Under busybox it showed as `free` rising by
about a megabyte for every process that exited, and a long session ending in
`Out of memory`. The `exits` line is the check that would have caught it: the
forking program runs four times to warm up, then four times in each of up to
four windows, and in one of them the free frame count must come back exactly
once the reaper has settled. One window is not enough, because what lives
across programs grows with how the processors happened to interleave; a leak
keeps frames in every window.

The program init starts is pid 1, as the first user process is on Linux:
ordinary numbering starts at 2, and each program of a command list takes 1 in
turn once the one before has let it go, so a shell as init reports `$$` as 1.
A process's children outlive it with a parent, handed on as Linux's
`forget_original_parent` hands them: to the nearest ancestor still running that
set `PR_SET_CHILD_SUBREAPER`, or else to init, each sent the signal it asked
for with `PR_SET_PDEATHSIG`. An orphan that had already ended is a zombie its
new parent's `wait4` takes, and one still running tells its new parent when it
ends. The boot check plays init with pid 1 and requires an ended and a running
orphan to reach it with their statuses, a reaping ancestor to take them first
and to pass them on once it has ended itself, and, as its control, the same
orphan to be left with no parent when there is no init.

**`futex` and `clone3`.** `futex` waits, wakes and requeues, plain and with a
bitset, with the word compared under the table's lock so no wake is lost. It
was keyed by address space and user address, so a futex in `MAP_SHARED`
memory was two futexes to the two sides of a `fork`; since 2026-09-23 a call
without `FUTEX_PRIVATE_FLAG` on a word in a shared region is keyed by the
object behind it and the word's offset there, as Linux keys it. The
priority-inheritance operations and
`FUTEX_WAKE_OP` are `ENOSYS`. `clone3` reads its argument structure by size and
takes the same path as `clone`, which is what glibc tries first and falls back
from only on `ENOSYS`. A process that ends clears its `clear_child_tid` word and
wakes whoever waits on it. The boot test catches a wake that rouses nobody,
and a waiter in a parent is roused by its fork child waking the same
`MAP_SHARED` word -- after a wake keyed by the child's own space, which is
how every wake was keyed before, has been shown to find nobody:

      futex    a changed word got EAGAIN and a timed wait ETIMEDOUT; a wake, a requeue and a wake from a fork child on a MAP_SHARED word roused 3 waiters, and a wake that roused nobody and one keyed by the waker's own space were caught

**inotify and pidfds, and real-time futex deadlines (ferrix-e4,
2026-09-27).** A black-box pass found `inotify_init`, `inotify_init1` and
`pidfd_open` answering `ENOSYS`. inotify (`src/kernel/src/fs/inotify.rs`) has
its four calls on all three architectures, a watch naming a node by device
and inode number, directory watches hearing their entries' events by name,
rename cookies, `IN_DELETE_SELF` with `IN_IGNORED`, the one-shot and mask
flags, merged repeats, `IN_Q_OVERFLOW`, poll, epoll and `FIONREAD`; nothing
it adds to a call costs anything until a watch exists. A pidfd
(`src/kernel/src/fs/pidfd.rs`) is readable once its process has ended, and
takes `pidfd_send_signal` and `waitid(P_PIDFD)`. A 59-check program run
first on the host's Linux passes on Ferrix. Left: a `siginfo_t` for
`pidfd_send_signal`, `CLONE_PIDFD`, `IN_UNMOUNT`, the per-user limits and
devfs's own nodes, which it makes without a call and so without
`IN_CREATE`. `io_uring_setup` stays `ENOSYS`: nothing on the roadmap uses
it, and what does falls back from `ENOSYS` as on a Linux built without it.
And an absolute `FUTEX_WAIT_BITSET` deadline on `FUTEX_CLOCK_REALTIME` is
now turned into the counter's as `clock_nanosleep` turns one; read as a
counter deadline it lay decades ahead, and glibc's
`pthread_cond_timedwait`, `sem_timedwait` and `pthread_mutex_timedlock`
never timed out.

**What the checks cost.** Stage 7 prints a `cost` line, guest milliseconds
per group of checks, as stage 5 does. It found the boot's single most
expensive check the day it was added: the futex negative control let its
forgotten waiter sleep out a two-second timeout on every boot, proving
nothing a 200 ms one does not. The whole stage now costs about 450 ms under
`tcg`, of which the handler checks and the two spinning programs are most.

A process's descriptors close when it ends, as Linux's exit closes them, not
when its parent reaps it: otherwise a pipe's write end outlives the program
that wrote, and `ls | wc -l` waits for an end of file that only reaping brings.

**The calls busybox makes around the edges.** Measured across both busyboxes'
applets and answered as a system without networking answered them then (the
socket families other than `AF_UNIX` came with *Networking*):
`prctl` (name, death signal, dumpable, no-new-privileges, subreaper, bounding
set), the robust-list head, resource limits (`RLIMIT_NOFILE` is the descriptor
table's own), priorities and I/O priorities, `personality`, the scheduler's
affinity and policy queries, `nanosleep`
and `clock_nanosleep`, `times` and `getrusage`, setting the real-time clock,
`adjtimex` queries, host and domain names, `sysinfo`, `getcpu`, `syslog` over
an empty log, `reboot` powering off, and every socket call for a family that is
not `AF_UNIX` refused as Linux without that address family refuses it. Their checks run in the handler group
with every structure's buffer poisoned beyond its end. Still `ENOSYS`, each
said so at its arm: swap, modules, System V shared memory and message
queues (each named in every architecture's table), `acct`, `vhangup` and
`rseq`. System V semaphores were among them until 2026-09-28, when the
customer decided to build them for the Steam client: `syscall/sem.rs`
answers `semget`, `semop`, `semtimedop` and `semctl` on every architecture
and i386's `ipc` (`docs/I386.md`), with `SEM_UNDO` paid at exit, each set,
undo record and blocked caller charged to its job (F-37) and at most
32,000 sets a job; the boot's `sem` line and `cargo xtask test-sem` prove
it.
**Credentials and file locks.** A process has real, effective, saved and
filesystem user and group ids and a supplementary group list. Fork copies
them; exec keeps them and makes the saved and filesystem ids the effective
ones, as `cap_bprm_creds_from_file` does, with `AT_SECURE` set when the
effective id is not the real one. The `set*id` calls, `setgroups` and `capget`
follow `kernel/sys.c` and `kernel/groups.c`, an effective uid of 0 standing in
for the capabilities, so busybox's `su` reaches a user.

**The ids are enforced**, which is what makes Ferrix multi-user rather than a
machine with ids written on it. `src/lib/fs/vfs`'s `access` holds the rules as pure
functions -- `generic_permission`, `may_create`, `may_delete` with the sticky
bit, `setattr_prepare`'s chown and chmod rules, `inode_init_owner` -- and the
namespace calls them where Linux does: search on every directory of a walk,
read or write on an open, execute on a program, write and search on the
directory of a create or a delete, and ownership for `chmod`, `chown` and
`utimensat`. A new file, pipe, socket or memfd belongs to its creator, a
process's `/proc` directory to the ids it acts as, and a set-user-id or
set-group-id program runs as its file's owner unless `PR_SET_NO_NEW_PRIVS`
forbade it. The calls that change the machine -- `sethostname`, `reboot`,
`mount`, `chroot`, `mknod` of a device, setting the clock -- and the calls
that reach another user's processes -- `kill`, `setpriority`, `prlimit64`,
`sched_setaffinity` -- refuse anyone but root, each with the error Linux
gives. `cargo xtask test-vfs` proves it from the user's side: the image
carries a `ferrix` user with a home of its own, `su` becomes it, and every
refusal above is required of the kernel, with root still able to read the
file it was refused.

`flock` locks
belong to the open file description, so a forked command keeps its parent's.
`fcntl` record locks come in both kinds: classic ones, owned by the
descriptor table and released by any close of the file, and open file
description locks, which end with the description; ranges split and merge,
`F_GETLK` names the holder, and `F_SETLKW` waits until a signal, with no
deadlock detection. Every path that ends a descriptor goes through
`fd::closed`, which is how a close releases them, and busybox's `adduser` and
`passwd` lock `/etc/passwd` rather than warning. `readahead` checks what
Linux checks and answers 0 without filling the page cache ahead.

**`mremap`, `execveat`, and what `/proc/self/exe` says.** `mremap` shrinks in
place, grows in place when the pages after are free, and otherwise moves --
a private mapping's frames moved into a new object of the new length rather
than copied, keeping protection and copy-on-write -- and refuses as Linux does,
checked with a string across a page boundary surviving the move. A fixed
destination below 64 KiB is `EPERM`, as from `mmap`, but only after the
overlap (`EINVAL`) and unmapped-source (`EFAULT`) refusals Linux reaches
first. `execveat`
shares `execve`'s path, `AT_EMPTY_PATH` included. `unshare` answers what a
process without namespaces can honestly answer, and `setns` refuses (mount
namespaces came with stage 13, `docs/NAMESPACES.md` N3). A program
is recorded as the absolute path of the file actually loaded, symlinks
resolved and a script's interpreter rather than the script, which is what
glibc's static start-up reads back through `/proc/self/exe`; `AT_EXECFN` is the
name `execve` was given. The shell init starts from its built-in
image, which has no file of its own, is named `/bin/busybox`, so the host's
static glibc busybox starts as init too. Since 2026-09-24 a fork child is
recorded as its parent was, as on Linux, and `/proc/<pid>/exe` is a magic
link that leads to the file itself, renamed or deleted, which is how Chrome
starts each child process (`docs/CHROME.md` §2.2).

**Signals are delivered.** `kill`, `tkill` and `tgkill` send; a child's end
sends its parent `SIGCHLD`; a write to a pipe with no reader raises `SIGPIPE`
and still returns `EPIPE`; `alarm` and `ITIMER_REAL` raise `SIGALRM`. Delivery
happens on every return to user mode, from a system call or a trap, into a
handler on Linux's own frame for each architecture -- floating-point state
included, `SA_ONSTACK`, `SA_NODEFER` and `SA_RESETHAND` honoured -- and
`rt_sigreturn` restores it with flags and processor mode sanitised, x86-64
returning through `IRETQ` because `SYSRET` cannot restore `rcx` and `r11`.
An interrupted blocking call is restarted when a handler with `SA_RESTART`
runs or when no handler runs, and is `EINTR` otherwise, exactly as Linux's
`arch_do_signal_or_restart` decides: reads, writes, `wait4`, pipe waits and
futex waits restart, `poll`, `select` and `pselect6` never do, and `nanosleep`
and `clock_nanosleep` resume through `restart_syscall` with the time left. A
default stop parks the process until `SIGCONT`, and a user-mode fault the fault
path cannot resolve becomes `SIGSEGV`, `SIGILL`, `SIGBUS`, `SIGFPE` or
`SIGTRAP` with Linux's codes rather than a kernel panic. The boot test runs a
handler that changes a register through its frame on each architecture, and a
`sigpaths` check drives the delivery decisions around it, each against a
negative control:

      signals  a program's handler ran on its own frame, changed a saved register, returned through sigreturn, and the program exited with 77
      sigpaths SIGCHLD reached a handler and wait4 still reaped; a stop and continue were reported; an alarm raised SIGALRM; SA_ONSTACK chose the alternate stack; a blocked fault was forced; SA_RESTART restarts, poll and a flagless handler do not

**The console is a terminal.** `fs/terminal.rs` holds the console's
`struct termios` and a line discipline that reads it for every byte --
canonical editing, echo, input and output mapping, `VMIN` and `VTIME` -- and
`syscall/tty.rs` answers the terminal requests, job control's included, so
busybox's `sh -i` gets a controlling terminal, turns job control on and does
its own line editing without a doubled echo. `select`, `pselect6` and
`pselect6_time64` answer from `poll`'s readiness and wait under the signal mask
they are given, as `ppoll` does. Ctrl-C, Ctrl-\ and Ctrl-Z raise their signals
on the foreground process group, and since nothing reads the console while a
shell waits for a foreground program, the first read starts a `console` thread
that drains the console every twenty milliseconds: the 4 KiB ring the PL011's,
the STM32 USART's and — through an I/O APIC input found from the MADT, since
2026-09-13 — the 16550's receive interrupts fill. A console read returns `EINTR`
when a signal is waiting. In the shell, `sleep 30` and `cat` are each ended by
Ctrl-C with status 130 and the prompt back at once.

**Unix-domain sockets, connected.** `fs/socket.rs` is a socket inode on a
sockfs of its own, built the way pipes are: each direction is
`src/lib/fs/vfs`'s `SocketBuffer` -- one queue as values, telling a stream from
records -- behind a lock, with a wait queue each way, and no wait ever happens
with the buffer locked. `socket` and `socketpair` make stream,
sequenced-packet and datagram sockets; `read`, `write`, `send`, `recv`,
`sendmsg` and `recvmsg` carry bytes and whole records between a pair, with
`MSG_DONTWAIT`, `MSG_PEEK`, `MSG_TRUNC`, `MSG_WAITALL` and `MSG_NOSIGNAL`,
one copy in and one out however many buffers a message names. `shutdown` ends
one direction at a time -- what was queued is still read, and the peer's sends
break -- `poll` answers from the two queues, and `FIONREAD` and `SIOCOUTQ`
report what a read would find and what a peer has not taken. `SO_TYPE`,
`SO_DOMAIN`, `SO_PROTOCOL`, `SO_ERROR`, `SO_ACCEPTCONN`, the buffer sizes
(kept doubled, as Linux keeps them), `SO_PEERCRED` and the two timeouts read
back; names came in the landing after. `SCM_RIGHTS` passes descriptors: a
send takes a reference to each named file and queues it with the first byte,
a receive installs as many as its control buffer has room for and closes the
rest, flagging `MSG_CTRUNC`, and a file is only ever dropped with the
descriptor table and the queue unlocked. Sockets passed over each other's
connections and then closed are collected at the last close, as Linux's
`unix_gc` collects them: a pass finds the sockets only queues refer to that no
readable queue holds, and empties their queues. A peek installs nothing where
Linux's installs duplicates.
The `unix` boot check drives a pair of each type through `dispatch`, and what
each call refuses stands beside what it answers -- the families and types
`AF_UNIX` is not, a call on the console, one on a closed descriptor, a peek
that took what it looked at, a record read that found the last record's tail:

      unix     a stream pair carried bytes across two writes and a peek left them; records kept their boundaries and MSG_TRUNC their lengths; shutdown ended one direction; a socket reported its type, buffers, credentials and unnamed address; a connection crossed an abstract name and a path, and 6 name calls were refused as specified; a descriptor travelled with a message, kept its file open in the queue, and was closed when a receive had no room for it; a cycle of sockets in flight was collected at its last close, and one a descriptor reached was kept

**Left, and why it did not block the exit:**

* **What signals do not do yet.** There is no vDSO, so a handler needs
  `SA_RESTORER`, which musl and glibc always set; only `ITIMER_REAL` arms, not
  `ITIMER_VIRTUAL` or `ITIMER_PROF`. `SA_RESTART`, `SIGCHLD` to a handler with
  `wait4` still reaping, job-control stop and continue, `alarm`/`ITIMER_REAL`,
  the alternate stack and the fault-to-signal path are now driven by the
  `sigpaths` boot check; the fault-to-signal catch and an `SA_RESTART`
  interrupted read are proven at the kernel's decision, not yet end-to-end by a
  hand-assembled faulting or interrupted-read user program.
* **Threads.** `clone(CLONE_THREAD)` makes a thread of the calling process:
  every program's task runs a `Thread`, whose id comes from the pid space and
  finds its process; signal state is split as Linux splits it, the thread
  taking its own signals before its process's; `exit` ends a thread and
  `exit_group` its process, which lets go of what it holds when its last
  thread has gone. Boot checks on all three architectures make threads in
  shared memory, clear `CLONE_CHILD_CLEARTID` as a thread ends, and end a
  process whose last two threads exit together -- since 2026-10-12 held at
  the kernel's live count between its read and its write until both have
  read, so that the two race on every run, and the old decision taken from
  the read alone fails that check by name; ferrousli's static pthread
  test runs to its end on x86-64. Signals work across threads: a signal sent
  to a process is judged across every live thread's mask and wakes one that
  can take it, `tkill`, `tgkill` and `SIGPIPE` reach one thread, a stop parks
  every thread and their blocked calls restart after `SIGCONT`, and `execve`
  from any thread ends the others and takes the pid -- each checked at boot on
  all three architectures. A futex wait reads its word under the table without
  faulting, and retries if another thread unmapped the page in between; `brk`
  and `fork` take a heap lock that may sleep, so a fork never copies a heap
  half shrunk; and an unmap on one processor waits for a copy on another to
  let go of its page. `/proc/<pid>/task` lists each thread with its `status`,
  `stat` and `comm`, and `Threads:` counts them. And the exit test runs:
  `cargo xtask test-threads` boots a static musl Rust program of five threads
  using `std::thread`, `Mutex` and `mpsc` as init on all three architectures,
  which counts its threads through `/proc/self`, with a build that expects
  one thread too many failing on that count. Since 2026-10-12 its last step
  shows the per-process credentials are safe under musl's `set*id`
  broadcast (`docs/POSIX-2024.md`): with a second thread alive, root's
  `setuid(1000)` survives musl repeating it on that thread, both read uid
  1000, and `setuid(0)` is then `EPERM`. It is linked as rustc links a
  musl program by default, which on x86-64 is a static PIE: the loader places
  an `ET_DYN` image without an interpreter at two thirds of the user half,
  with its entry and `AT_PHDR` moved, and the program relocates itself; a
  boot check loads such an image on all three architectures. A dynamically
  linked program, which names an interpreter, was refused here; since
  2026-09-20 the kernel loads the linker it names and enters it (*Dynamic
  linking*).
* **Three stand-ins, each written down where it lives, and one left.** The
  console's line discipline is fed by a thread that looks every twenty
  milliseconds rather than waiting on the receive interrupt, and still is.
  The other two are gone since 2026-09-16: the real-time clocks start at
  the time firmware's clock gave the loader rather than at 1970, and
  `getrandom` is `src/lib/kernel/crng`'s ChaCha20, seeded from firmware's
  `EFI_RNG_PROTOCOL`, the processor's generator and jitter. And the console
  is no longer the one terminal: pseudo-terminals came with stage 18.

---

