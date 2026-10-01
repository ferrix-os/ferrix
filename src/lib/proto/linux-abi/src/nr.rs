//! System call numbers, and the translation from a number to a call.
//!
//! There is no single Linux system call table. Ferrix's three architectures
//! use three of them: x86-64 kept the numbering it grew
//! (`arch/x86/entry/syscalls/syscall_64.tbl`), every architecture added after
//! 2011 -- AArch64 included -- uses the generic table in
//! `include/uapi/asm-generic/unistd.h`, and ARMv7-A carries the EABI table in
//! `arch/arm/include/uapi/asm/unistd-common.h`, which predates both. The same
//! call has a different number on each, some calls exist on only one, and a
//! binary compiled for one architecture never sees the others' numbers.
//!
//! A fourth arrives on x86-64 itself: a 32-bit program running there calls
//! through `int $0x80` with i386's numbers (`arch/x86/entry/syscalls/
//! syscall_32.tbl`), and [`from_i386`] reads those (`docs/I386.md`).
//!
//! The kernel should not care. [`from_x86_64`], [`from_aarch64`],
//! [`from_arm`] and [`from_i386`] map each table's raw number onto one [`Syscall`], and
//! the dispatcher matches on that. An unknown number maps to [`None`], which
//! the caller answers with [`crate::errno::Errno::ENOSYS`] — the same thing
//! Linux does.
//!
//! # The 32-bit architecture is not just a third column
//!
//! ARMv7-A does not merely renumber the same calls. A 32-bit register cannot
//! carry a file offset, a file size or a post-2038 `time_t`, so the calls that
//! pass one exist twice, and the wider form is a *different call with a
//! different signature*: [`Syscall::Mmap2`] counts its offset in pages rather
//! than bytes, [`Syscall::Llseek`] returns its result through a pointer, and
//! [`Syscall::Fstat64`] writes a structure `fstat` would not recognise. Those
//! are separate [`Syscall`] variants for exactly that reason -- folding them
//! onto their 64-bit namesakes would hand a handler arguments it would
//! silently misread.
//!
//! # What is here
//!
//! Enough of the table for a static-musl program to start and run: process and
//! thread creation, memory, files, signals, futexes, time and the handful of
//! socket calls musl uses. Calls whose numbers were not verified against the
//! kernel source are absent rather than guessed, because a wrong number is a
//! call silently dispatched to the wrong handler, while a missing one is an
//! `ENOSYS` that names itself.
//!
//! The pre-2.4 16-bit credential calls (`getuid`, `setuid`, `setresuid` and
//! their kin, scattered between ARMv7-A numbers 23 and 171) are deliberately
//! absent for the same reason: musl
//! issues only the `32` forms, so a call arriving on one of the old numbers is
//! more likely a mistake than a request, and `ENOSYS` says so.

/// System call numbers for x86-64.
///
/// From `arch/x86/entry/syscalls/syscall_64.tbl`, the 64-bit ABI rather than
/// the x32 one.
pub mod x86_64 {
    /// Read bytes from a file descriptor.
    pub const READ: usize = 0;
    /// Write bytes to a file descriptor.
    pub const WRITE: usize = 1;
    /// Open a file by path. AArch64 has no equivalent; musl uses `openat`
    /// there. ARMv7-A kept it, at a number of its own.
    pub const OPEN: usize = 2;
    /// Close a file descriptor.
    pub const CLOSE: usize = 3;
    /// Stat a file by path, following symlinks.
    pub const STAT: usize = 4;
    /// Stat an open file descriptor.
    pub const FSTAT: usize = 5;
    /// Stat a file by path without following a final symlink.
    pub const LSTAT: usize = 6;
    /// Wait for events on a set of file descriptors.
    pub const POLL: usize = 7;
    /// Reposition a file descriptor's offset.
    pub const LSEEK: usize = 8;
    /// Map files or anonymous memory into the address space.
    pub const MMAP: usize = 9;
    /// Change the protection of a mapping.
    pub const MPROTECT: usize = 10;
    /// Remove a mapping.
    pub const MUNMAP: usize = 11;
    /// Move the program break, the classic heap boundary.
    pub const BRK: usize = 12;
    /// Install a signal handler.
    pub const RT_SIGACTION: usize = 13;
    /// Change the blocked signal mask.
    pub const RT_SIGPROCMASK: usize = 14;
    /// Return from a signal handler, restoring the interrupted context.
    pub const RT_SIGRETURN: usize = 15;
    /// Resume a system call interrupted by a signal handler. The kernel points
    /// a restarted call's number register here; no program issues it itself.
    pub const RESTART_SYSCALL: usize = 219;
    /// Device-specific control operation on a file descriptor.
    pub const IOCTL: usize = 16;
    /// Read at an explicit offset, leaving the file position alone.
    pub const PREAD64: usize = 17;
    /// Write at an explicit offset, leaving the file position alone.
    pub const PWRITE64: usize = 18;
    /// Read into several buffers in one call.
    pub const READV: usize = 19;
    /// Write from several buffers in one call.
    pub const WRITEV: usize = 20;
    /// Check a path's accessibility for the real user.
    pub const ACCESS: usize = 21;
    /// Create a pipe, returning two file descriptors.
    pub const PIPE: usize = 22;
    /// Wait for readiness on sets of file descriptors, with a `timeval` timeout.
    pub const SELECT: usize = 23;
    /// Yield the processor to another runnable thread.
    pub const SCHED_YIELD: usize = 24;
    /// Resize, and possibly move, an existing mapping.
    pub const MREMAP: usize = 25;
    /// Flush a file-backed mapping to its file.
    pub const MSYNC: usize = 26;
    /// Report which pages of a mapping are resident.
    pub const MINCORE: usize = 27;
    /// Advise the kernel about future use of a memory range.
    pub const MADVISE: usize = 28;
    /// Create or look up a System V shared memory segment.
    pub const SHMGET: usize = 29;
    /// Attach a System V shared memory segment.
    pub const SHMAT: usize = 30;
    /// Query or control a System V shared memory segment.
    pub const SHMCTL: usize = 31;
    /// Duplicate a file descriptor onto the lowest free number.
    pub const DUP: usize = 32;
    /// Duplicate a file descriptor onto a chosen number.
    pub const DUP2: usize = 33;
    /// Sleep until a signal arrives.
    pub const PAUSE: usize = 34;
    /// Sleep for a duration, resumable after a signal.
    pub const NANOSLEEP: usize = 35;
    /// Read an interval timer.
    pub const GETITIMER: usize = 36;
    /// Arrange for `SIGALRM` after a number of seconds.
    pub const ALARM: usize = 37;
    /// Arm or disarm an interval timer.
    pub const SETITIMER: usize = 38;
    /// Return the calling process's identifier.
    pub const GETPID: usize = 39;
    /// Copy data between two file descriptors inside the kernel.
    pub const SENDFILE: usize = 40;
    /// Create a socket.
    pub const SOCKET: usize = 41;
    /// Connect a socket to an address.
    pub const CONNECT: usize = 42;
    /// Accept a connection on a listening socket.
    pub const ACCEPT: usize = 43;
    /// Send a message on a socket, optionally to an address.
    pub const SENDTO: usize = 44;
    /// Receive a message from a socket, with its source address.
    pub const RECVFROM: usize = 45;
    /// Send a message with ancillary data on a socket.
    pub const SENDMSG: usize = 46;
    /// Receive a message with ancillary data from a socket.
    pub const RECVMSG: usize = 47;
    /// Shut down part or all of a full-duplex connection.
    pub const SHUTDOWN: usize = 48;
    /// Bind a socket to a local address.
    pub const BIND: usize = 49;
    /// Mark a socket as accepting connections.
    pub const LISTEN: usize = 50;
    /// Read a socket's local address.
    pub const GETSOCKNAME: usize = 51;
    /// Read the address of a socket's peer.
    pub const GETPEERNAME: usize = 52;
    /// Create a pair of connected sockets.
    pub const SOCKETPAIR: usize = 53;
    /// Set a socket option.
    pub const SETSOCKOPT: usize = 54;
    /// Read a socket option.
    pub const GETSOCKOPT: usize = 55;
    /// Create a process or thread; the primitive behind `fork` and `pthread`.
    pub const CLONE: usize = 56;
    /// Create a child process sharing nothing.
    pub const FORK: usize = 57;
    /// Create a child sharing the address space, suspending the parent.
    pub const VFORK: usize = 58;
    /// Replace the current process image with a program.
    pub const EXECVE: usize = 59;
    /// Terminate the calling thread.
    pub const EXIT: usize = 60;
    /// Wait for a child to change state, reporting resource usage.
    pub const WAIT4: usize = 61;
    /// Send a signal to a process or process group.
    pub const KILL: usize = 62;
    /// Report kernel name and version.
    pub const UNAME: usize = 63;
    /// Create or look up a System V semaphore set.
    pub const SEMGET: usize = 64;
    /// Operate on the semaphores in a System V set.
    pub const SEMOP: usize = 65;
    /// Query or control a System V semaphore set.
    pub const SEMCTL: usize = 66;
    /// Detach a System V shared memory segment.
    pub const SHMDT: usize = 67;
    /// Create or look up a System V message queue.
    pub const MSGGET: usize = 68;
    /// Send a message to a System V message queue.
    pub const MSGSND: usize = 69;
    /// Receive a message from a System V message queue.
    pub const MSGRCV: usize = 70;
    /// Query or control a System V message queue.
    pub const MSGCTL: usize = 71;
    /// Manipulate a file descriptor's flags and locks.
    pub const FCNTL: usize = 72;
    /// Take or release an advisory lock on a whole open file.
    pub const FLOCK: usize = 73;
    /// Flush a file's data and metadata to storage.
    pub const FSYNC: usize = 74;
    /// Flush a file's data, and only the metadata needed to read it back.
    pub const FDATASYNC: usize = 75;
    /// Set a file's length by path.
    pub const TRUNCATE: usize = 76;
    /// Set an open file's length.
    pub const FTRUNCATE: usize = 77;
    /// Read the current working directory into a buffer.
    pub const GETCWD: usize = 79;
    /// Change the working directory by path.
    pub const CHDIR: usize = 80;
    /// Change the working directory to an open directory.
    pub const FCHDIR: usize = 81;
    /// Rename a file.
    pub const RENAME: usize = 82;
    /// Create a directory.
    pub const MKDIR: usize = 83;
    /// Remove an empty directory.
    pub const RMDIR: usize = 84;
    /// Create a file, or truncate it, and open it for writing: `open` with
    /// `O_CREAT | O_WRONLY | O_TRUNC`. x86-64 and ARMv7-A only.
    pub const CREAT: usize = 85;
    /// Create a hard link.
    pub const LINK: usize = 86;
    /// Remove a directory entry.
    pub const UNLINK: usize = 87;
    /// Create a symbolic link.
    pub const SYMLINK: usize = 88;
    /// Read a symbolic link's target.
    pub const READLINK: usize = 89;
    /// Change a file's mode by path.
    pub const CHMOD: usize = 90;
    /// Change an open file's mode.
    pub const FCHMOD: usize = 91;
    /// Change a file's owner by path.
    pub const CHOWN: usize = 92;
    /// Change an open file's owner.
    pub const FCHOWN: usize = 93;
    /// Change a file's owner by path, without following a final symlink.
    pub const LCHOWN: usize = 94;
    /// Set the file mode creation mask.
    pub const UMASK: usize = 95;
    /// Read the wall clock, the obsolete predecessor of `clock_gettime`.
    pub const GETTIMEOFDAY: usize = 96;
    /// Read a resource limit; superseded by `prlimit64`.
    pub const GETRLIMIT: usize = 97;
    /// Report accumulated resource usage.
    pub const GETRUSAGE: usize = 98;
    /// Report system-wide memory and load statistics.
    pub const SYSINFO: usize = 99;
    /// Report the CPU time used by the process and its children.
    pub const TIMES: usize = 100;
    /// Return the real user identifier.
    pub const GETUID: usize = 102;
    /// Read or control the kernel log buffer.
    pub const SYSLOG: usize = 103;
    /// Return the real group identifier.
    pub const GETGID: usize = 104;
    /// Set the user identifier.
    pub const SETUID: usize = 105;
    /// Set the group identifier.
    pub const SETGID: usize = 106;
    /// Return the effective user identifier.
    pub const GETEUID: usize = 107;
    /// Return the effective group identifier.
    pub const GETEGID: usize = 108;
    /// Set a process's process-group identifier.
    pub const SETPGID: usize = 109;
    /// Return the parent process's identifier.
    pub const GETPPID: usize = 110;
    /// Return the calling process's process-group identifier.
    pub const GETPGRP: usize = 111;
    /// Start a new session, with the caller as its leader.
    pub const SETSID: usize = 112;
    /// Set the real and effective user identifiers.
    pub const SETREUID: usize = 113;
    /// Set the real and effective group identifiers.
    pub const SETREGID: usize = 114;
    /// Read the supplementary group list.
    pub const GETGROUPS: usize = 115;
    /// Replace the supplementary group list.
    pub const SETGROUPS: usize = 116;
    /// Set the real, effective and saved user identifiers.
    pub const SETRESUID: usize = 117;
    /// Read the real, effective and saved user identifiers.
    pub const GETRESUID: usize = 118;
    /// Set the real, effective and saved group identifiers.
    pub const SETRESGID: usize = 119;
    /// Read the real, effective and saved group identifiers.
    pub const GETRESGID: usize = 120;
    /// Return a process's process-group identifier.
    pub const GETPGID: usize = 121;
    /// Set the user identifier used for file access checks.
    pub const SETFSUID: usize = 122;
    /// Set the group identifier used for file access checks.
    pub const SETFSGID: usize = 123;
    /// Return a process's session identifier.
    pub const GETSID: usize = 124;
    /// Read a thread's capability sets.
    pub const CAPGET: usize = 125;
    /// Set a thread's capability sets.
    pub const CAPSET: usize = 126;
    /// Report the blocked signals that are pending.
    pub const RT_SIGPENDING: usize = 127;
    /// Wait for one of a set of signals, with a timeout.
    pub const RT_SIGTIMEDWAIT: usize = 128;
    /// Send a signal with caller-supplied `siginfo` to a process.
    pub const RT_SIGQUEUEINFO: usize = 129;
    /// Replace the signal mask and wait for a signal.
    pub const RT_SIGSUSPEND: usize = 130;
    /// Install or query the alternate signal stack.
    pub const SIGALTSTACK: usize = 131;
    /// Create a file, device node, pipe or socket name by path.
    pub const MKNOD: usize = 133;
    /// Read or set the process's execution domain.
    pub const PERSONALITY: usize = 135;
    /// Report file system statistics by path.
    pub const STATFS: usize = 137;
    /// Report file system statistics for an open file.
    pub const FSTATFS: usize = 138;
    /// Read the nice value of a process, process group or user.
    pub const GETPRIORITY: usize = 140;
    /// Set the nice value of a process, process group or user.
    pub const SETPRIORITY: usize = 141;
    /// Set a thread's scheduling parameters.
    pub const SCHED_SETPARAM: usize = 142;
    /// Read a thread's scheduling parameters.
    pub const SCHED_GETPARAM: usize = 143;
    /// Set a thread's scheduling policy and parameters.
    pub const SCHED_SETSCHEDULER: usize = 144;
    /// Read a thread's scheduling policy.
    pub const SCHED_GETSCHEDULER: usize = 145;
    /// Report the highest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MAX: usize = 146;
    /// Report the lowest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MIN: usize = 147;
    /// Report a thread's round-robin time slice.
    pub const SCHED_RR_GET_INTERVAL: usize = 148;
    /// Simulate a hangup on the calling process's terminal.
    pub const VHANGUP: usize = 153;
    /// Operate on per-process control settings, such as the thread name.
    pub const PRCTL: usize = 157;
    /// Read or set an architecture register, notably `FS_BASE` for TLS.
    ///
    /// x86-64 only. AArch64 writes its thread pointer to `TPIDR_EL0` directly,
    /// so the generic table never needed the call; ARMv7-A cannot write its
    /// own either, and asks through [`super::arm::ARM_SET_TLS`] instead.
    pub const ARCH_PRCTL: usize = 158;
    /// Read or tune the system clock's discipline.
    pub const ADJTIMEX: usize = 159;
    /// Set a resource limit; superseded by `prlimit64`.
    pub const SETRLIMIT: usize = 160;
    /// Flush all file systems.
    pub const SYNC: usize = 162;
    /// Turn process accounting on or off.
    pub const ACCT: usize = 163;
    /// Set the wall clock, the obsolete predecessor of `clock_settime`.
    pub const SETTIMEOFDAY: usize = 164;
    /// Attach a file system.
    pub const MOUNT: usize = 165;
    /// Detach a file system.
    pub const UMOUNT2: usize = 166;
    /// Start swapping to a file or device.
    pub const SWAPON: usize = 167;
    /// Stop swapping to a file or device.
    pub const SWAPOFF: usize = 168;
    /// Reboot, halt or power off the machine.
    pub const REBOOT: usize = 169;
    /// Set the host name `uname` reports.
    pub const SETHOSTNAME: usize = 170;
    /// Set the NIS domain name `uname` reports.
    pub const SETDOMAINNAME: usize = 171;
    /// Load a kernel module from a buffer.
    pub const INIT_MODULE: usize = 175;
    /// Unload a kernel module.
    pub const DELETE_MODULE: usize = 176;
    /// Return the calling thread's identifier.
    pub const GETTID: usize = 186;
    /// Ask for a range of a file to be read into the page cache ahead of use.
    pub const READAHEAD: usize = 187;
    /// Send a signal to a thread by thread identifier.
    pub const TKILL: usize = 200;
    /// Read the wall clock in whole seconds, older still than `gettimeofday`.
    pub const TIME: usize = 201;
    /// Wait on, or wake, a futex; the primitive under every musl lock.
    pub const FUTEX: usize = 202;
    /// Set a thread's processor affinity mask.
    pub const SCHED_SETAFFINITY: usize = 203;
    /// Read a thread's processor affinity mask.
    pub const SCHED_GETAFFINITY: usize = 204;
    /// Read directory entries in the 64-bit layout.
    pub const GETDENTS64: usize = 217;
    /// Register the address cleared and woken on thread exit.
    pub const SET_TID_ADDRESS: usize = 218;
    /// Operate on System V semaphores, giving up after a timeout.
    pub const SEMTIMEDOP: usize = 220;
    /// Set a clock.
    pub const CLOCK_SETTIME: usize = 227;
    /// Read a clock.
    pub const CLOCK_GETTIME: usize = 228;
    /// Read a clock's resolution.
    pub const CLOCK_GETRES: usize = 229;
    /// Sleep against a chosen clock, optionally until an absolute time.
    pub const CLOCK_NANOSLEEP: usize = 230;
    /// Terminate every thread in the process.
    pub const EXIT_GROUP: usize = 231;
    /// Create an epoll set, the size a hint that is ignored.
    pub const EPOLL_CREATE: usize = 213;
    /// Wait for events on an epoll set.
    pub const EPOLL_WAIT: usize = 232;
    /// Add, modify or remove a file descriptor in an epoll set.
    pub const EPOLL_CTL: usize = 233;
    /// Send a signal to a thread, checked against its thread group.
    pub const TGKILL: usize = 234;
    /// Wait for a child to change state, without necessarily reaping it.
    pub const WAITID: usize = 247;
    /// Set a process's I/O scheduling class and priority.
    pub const IOPRIO_SET: usize = 251;
    /// Read a process's I/O scheduling class and priority.
    pub const IOPRIO_GET: usize = 252;
    /// Make an inotify instance, with no flags. x86-64 and ARMv7-A only.
    pub const INOTIFY_INIT: usize = 253;
    /// Watch a path on an inotify instance.
    pub const INOTIFY_ADD_WATCH: usize = 254;
    /// Remove a watch from an inotify instance.
    pub const INOTIFY_RM_WATCH: usize = 255;
    /// Open a file relative to a directory file descriptor.
    pub const OPENAT: usize = 257;
    /// Create a directory relative to a directory file descriptor.
    pub const MKDIRAT: usize = 258;
    /// Create a file, device node, pipe or socket name relative to a
    /// directory file descriptor.
    pub const MKNODAT: usize = 259;
    /// Change a file's owner relative to a directory file descriptor.
    pub const FCHOWNAT: usize = 260;
    /// Stat a file relative to a directory file descriptor.
    pub const NEWFSTATAT: usize = 262;
    /// Remove a directory entry relative to a directory file descriptor.
    pub const UNLINKAT: usize = 263;
    /// Rename relative to directory file descriptors.
    pub const RENAMEAT: usize = 264;
    /// Create a hard link relative to directory file descriptors.
    pub const LINKAT: usize = 265;
    /// Create a symbolic link relative to a directory file descriptor.
    pub const SYMLINKAT: usize = 266;
    /// Read a symbolic link relative to a directory file descriptor.
    pub const READLINKAT: usize = 267;
    /// Change a file's mode relative to a directory file descriptor.
    pub const FCHMODAT: usize = 268;
    /// Check accessibility relative to a directory file descriptor.
    pub const FACCESSAT: usize = 269;
    /// Wait for readiness on descriptor sets, with a signal mask and a `timespec`.
    pub const PSELECT6: usize = 270;
    /// Poll with a signal mask and a `timespec` timeout.
    pub const PPOLL: usize = 271;
    /// Detach parts of the calling process's shared execution context.
    pub const UNSHARE: usize = 272;
    /// Register the robust futex list, walked when a thread dies holding a lock.
    pub const SET_ROBUST_LIST: usize = 273;
    /// Read a thread's robust futex list.
    pub const GET_ROBUST_LIST: usize = 274;
    /// Move bytes between a pipe and another descriptor inside the kernel.
    pub const SPLICE: usize = 275;
    /// Set a file's access and modification times, to the nanosecond.
    pub const UTIMENSAT: usize = 280;
    /// Wait on an epoll set with a signal mask.
    pub const EPOLL_PWAIT: usize = 281;
    /// Create a signalfd, or change one's mask, with no flags.
    pub const SIGNALFD: usize = 282;
    /// Create a timer that expires into a descriptor.
    pub const TIMERFD_CREATE: usize = 283;
    /// Create an eventfd with no flags.
    pub const EVENTFD: usize = 284;
    /// Arm or disarm a timerfd.
    pub const TIMERFD_SETTIME: usize = 286;
    /// Read a timerfd's time left and interval.
    pub const TIMERFD_GETTIME: usize = 287;
    /// Accept a connection, with flags for the new descriptor.
    pub const ACCEPT4: usize = 288;
    /// Create a signalfd, or change one's mask, with flags.
    pub const SIGNALFD4: usize = 289;
    /// Create an eventfd with flags.
    pub const EVENTFD2: usize = 290;
    /// Create an epoll set with flags.
    pub const EPOLL_CREATE1: usize = 291;
    /// Duplicate a file descriptor onto a chosen number with flags.
    pub const DUP3: usize = 292;
    /// Create a pipe with flags.
    pub const PIPE2: usize = 293;
    /// Make an inotify instance, with flags.
    pub const INOTIFY_INIT1: usize = 294;
    /// Read and set a resource limit of any process in one call.
    pub const PRLIMIT64: usize = 302;
    /// Encode a file handle for a path; opening one back is `open_by_handle_at`.
    pub const NAME_TO_HANDLE_AT: usize = 303;
    /// Read or tune a chosen clock's discipline.
    pub const CLOCK_ADJTIME: usize = 305;
    /// Move the calling thread into an existing namespace.
    pub const SETNS: usize = 308;
    /// Report the processor and NUMA node the caller is running on.
    pub const GETCPU: usize = 309;
    /// Load a kernel module from a file descriptor.
    pub const FINIT_MODULE: usize = 313;
    /// Rename with flags, such as `RENAME_NOREPLACE`.
    pub const RENAMEAT2: usize = 316;
    /// Restrict the system calls a thread may make: `seccomp(2)`.
    pub const SECCOMP: usize = 317;
    /// Fill a buffer with random bytes.
    pub const GETRANDOM: usize = 318;
    /// Create an anonymous file living in memory.
    pub const MEMFD_CREATE: usize = 319;
    /// Execute a program named by a file descriptor.
    pub const EXECVEAT: usize = 322;
    /// Issue a process-wide memory barrier.
    pub const MEMBARRIER: usize = 324;
    /// Copy a range of one regular file into another inside the kernel.
    pub const COPY_FILE_RANGE: usize = 326;
    /// Stat a file with an explicit field mask and 64-bit timestamps.
    pub const STATX: usize = 332;
    /// Register a restartable sequence area.
    pub const RSEQ: usize = 334;
    /// Send a signal to the process a pidfd refers to.
    pub const PIDFD_SEND_SIGNAL: usize = 424;
    /// Make a pidfd: a descriptor for a process, readable once it has ended.
    pub const PIDFD_OPEN: usize = 434;
    /// Create a process or thread from a versioned argument structure.
    pub const CLONE3: usize = 435;
    /// Open a file from a versioned argument structure.
    pub const OPENAT2: usize = 437;
    /// Check accessibility with flags, the form musl now prefers.
    pub const FACCESSAT2: usize = 439;
    /// Wait on an epoll set with a signal mask and a nanosecond timeout.
    pub const EPOLL_PWAIT2: usize = 441;

    // -- Filesystem control and extended attributes, from `syscall_64.tbl` --

    /// Set an extended attribute by path, following symbolic links.
    pub const SETXATTR: usize = 188;
    /// Set an extended attribute by path, on a symbolic link itself.
    pub const LSETXATTR: usize = 189;
    /// Set an extended attribute of an open file.
    pub const FSETXATTR: usize = 190;
    /// Read an extended attribute by path, following symbolic links.
    pub const GETXATTR: usize = 191;
    /// Read an extended attribute by path, of a symbolic link itself.
    pub const LGETXATTR: usize = 192;
    /// Read an extended attribute of an open file.
    pub const FGETXATTR: usize = 193;
    /// List extended attribute names by path, following symbolic links.
    pub const LISTXATTR: usize = 194;
    /// List extended attribute names by path, of a symbolic link itself.
    pub const LLISTXATTR: usize = 195;
    /// List extended attribute names of an open file.
    pub const FLISTXATTR: usize = 196;
    /// Remove an extended attribute by path, following symbolic links.
    pub const REMOVEXATTR: usize = 197;
    /// Remove an extended attribute by path, from a symbolic link itself.
    pub const LREMOVEXATTR: usize = 198;
    /// Remove an extended attribute of an open file.
    pub const FREMOVEXATTR: usize = 199;
    /// Make another mount the root, and move the old root beneath it.
    pub const PIVOT_ROOT: usize = 155;
    /// Reserve or release storage for a range of an open file.
    pub const FALLOCATE: usize = 285;
    /// Change the calling process's root directory.
    pub const CHROOT: usize = 161;
    /// Flush the file system holding an open file.
    pub const SYNCFS: usize = 306;
}

/// System call numbers for AArch64.
///
/// From `include/uapi/asm-generic/unistd.h`, which AArch64 uses unmodified.
/// The table deliberately omits the path-based calls that have an `*at`
/// equivalent, so there is no `open`, `stat`, `lstat`, `poll`, `pipe`, `dup2`,
/// `fork`, `vfork`, `access`, `rename`, `mkdir`, `rmdir`, `unlink`, `symlink`,
/// `readlink`, `chmod` or `chown` here — musl calls the `*at` forms. There is
/// no `arch_prctl` either, since the thread pointer is a writable register, and
/// no `alarm`, `pause`, `select` or `getpgrp`: musl reaches those through
/// `setitimer`, `ppoll`, `pselect6` and `getpgid(0)`.
pub mod aarch64 {
    /// Read the current working directory into a buffer.
    pub const GETCWD: usize = 17;
    /// Create an eventfd with flags.
    pub const EVENTFD2: usize = 19;
    /// Create an epoll set with flags.
    pub const EPOLL_CREATE1: usize = 20;
    /// Add, modify or remove a file descriptor in an epoll set.
    pub const EPOLL_CTL: usize = 21;
    /// Wait on an epoll set with a signal mask.
    ///
    /// The generic table has no plain `epoll_wait`: musl passes a null mask.
    pub const EPOLL_PWAIT: usize = 22;
    /// Duplicate a file descriptor onto the lowest free number.
    pub const DUP: usize = 23;
    /// Duplicate a file descriptor onto a chosen number with flags.
    pub const DUP3: usize = 24;
    /// Manipulate a file descriptor's flags and locks.
    pub const FCNTL: usize = 25;
    /// Make an inotify instance, with flags.
    pub const INOTIFY_INIT1: usize = 26;
    /// Watch a path on an inotify instance.
    pub const INOTIFY_ADD_WATCH: usize = 27;
    /// Remove a watch from an inotify instance.
    pub const INOTIFY_RM_WATCH: usize = 28;
    /// Device-specific control operation on a file descriptor.
    pub const IOCTL: usize = 29;
    /// Set a process's I/O scheduling class and priority.
    pub const IOPRIO_SET: usize = 30;
    /// Read a process's I/O scheduling class and priority.
    pub const IOPRIO_GET: usize = 31;
    /// Take or release an advisory lock on a whole open file.
    pub const FLOCK: usize = 32;
    /// Create a file, device node, pipe or socket name relative to a
    /// directory file descriptor.
    pub const MKNODAT: usize = 33;
    /// Create a directory relative to a directory file descriptor.
    pub const MKDIRAT: usize = 34;
    /// Remove a directory entry relative to a directory file descriptor.
    pub const UNLINKAT: usize = 35;
    /// Create a symbolic link relative to a directory file descriptor.
    pub const SYMLINKAT: usize = 36;
    /// Create a hard link relative to directory file descriptors.
    pub const LINKAT: usize = 37;
    /// Rename relative to directory file descriptors.
    pub const RENAMEAT: usize = 38;
    /// Rename a file, with flags such as `RENAME_NOREPLACE`.
    pub const RENAMEAT2: usize = 276;
    /// Detach a file system.
    pub const UMOUNT2: usize = 39;
    /// Attach a file system.
    pub const MOUNT: usize = 40;
    /// Report file system statistics by path.
    pub const STATFS: usize = 43;
    /// Report file system statistics for an open file.
    pub const FSTATFS: usize = 44;
    /// Set a file's length by path.
    pub const TRUNCATE: usize = 45;
    /// Set an open file's length.
    pub const FTRUNCATE: usize = 46;
    /// Check accessibility relative to a directory file descriptor.
    pub const FACCESSAT: usize = 48;
    /// Change the working directory by path.
    pub const CHDIR: usize = 49;
    /// Change the working directory to an open directory.
    pub const FCHDIR: usize = 50;
    /// Change an open file's mode.
    pub const FCHMOD: usize = 52;
    /// Change a file's mode relative to a directory file descriptor.
    pub const FCHMODAT: usize = 53;
    /// Change a file's owner relative to a directory file descriptor.
    pub const FCHOWNAT: usize = 54;
    /// Change an open file's owner.
    pub const FCHOWN: usize = 55;
    /// Open a file relative to a directory file descriptor.
    pub const OPENAT: usize = 56;
    /// Close a file descriptor.
    pub const CLOSE: usize = 57;
    /// Simulate a hangup on the calling process's terminal.
    pub const VHANGUP: usize = 58;
    /// Create a pipe with flags.
    pub const PIPE2: usize = 59;
    /// Read directory entries in the 64-bit layout.
    pub const GETDENTS64: usize = 61;
    /// Reposition a file descriptor's offset.
    pub const LSEEK: usize = 62;
    /// Read bytes from a file descriptor.
    pub const READ: usize = 63;
    /// Write bytes to a file descriptor.
    pub const WRITE: usize = 64;
    /// Read into several buffers in one call.
    pub const READV: usize = 65;
    /// Write from several buffers in one call.
    pub const WRITEV: usize = 66;
    /// Read at an explicit offset, leaving the file position alone.
    pub const PREAD64: usize = 67;
    /// Write at an explicit offset, leaving the file position alone.
    pub const PWRITE64: usize = 68;
    /// Copy data between two file descriptors inside the kernel.
    pub const SENDFILE: usize = 71;
    /// Wait for readiness on descriptor sets, with a signal mask and a `timespec`.
    pub const PSELECT6: usize = 72;
    /// Poll with a signal mask and a `timespec` timeout.
    pub const PPOLL: usize = 73;
    /// Create a signalfd, or change one's mask, with flags.
    pub const SIGNALFD4: usize = 74;
    /// Move bytes between a pipe and another descriptor inside the kernel.
    pub const SPLICE: usize = 76;
    /// Read a symbolic link relative to a directory file descriptor.
    pub const READLINKAT: usize = 78;
    /// Stat a file relative to a directory file descriptor.
    pub const NEWFSTATAT: usize = 79;
    /// Stat an open file descriptor.
    pub const FSTAT: usize = 80;
    /// Flush all file systems.
    pub const SYNC: usize = 81;
    /// Flush a file's data and metadata to storage.
    pub const FSYNC: usize = 82;
    /// Flush a file's data, and only the metadata needed to read it back.
    pub const FDATASYNC: usize = 83;
    /// Create a timer that expires into a descriptor.
    pub const TIMERFD_CREATE: usize = 85;
    /// Arm or disarm a timerfd.
    pub const TIMERFD_SETTIME: usize = 86;
    /// Read a timerfd's time left and interval.
    pub const TIMERFD_GETTIME: usize = 87;
    /// Set a file's access and modification times, to the nanosecond.
    pub const UTIMENSAT: usize = 88;
    /// Turn process accounting on or off.
    pub const ACCT: usize = 89;
    /// Read a thread's capability sets.
    pub const CAPGET: usize = 90;
    /// Set a thread's capability sets.
    pub const CAPSET: usize = 91;
    /// Read or set the process's execution domain.
    pub const PERSONALITY: usize = 92;
    /// Terminate the calling thread.
    pub const EXIT: usize = 93;
    /// Terminate every thread in the process.
    pub const EXIT_GROUP: usize = 94;
    /// Wait for a child to change state, without necessarily reaping it.
    pub const WAITID: usize = 95;
    /// Register the address cleared and woken on thread exit.
    pub const SET_TID_ADDRESS: usize = 96;
    /// Detach parts of the calling process's shared execution context.
    pub const UNSHARE: usize = 97;
    /// Wait on, or wake, a futex; the primitive under every musl lock.
    pub const FUTEX: usize = 98;
    /// Register the robust futex list, walked when a thread dies holding a lock.
    pub const SET_ROBUST_LIST: usize = 99;
    /// Read a thread's robust futex list.
    pub const GET_ROBUST_LIST: usize = 100;
    /// Sleep for a duration, resumable after a signal.
    pub const NANOSLEEP: usize = 101;
    /// Read an interval timer.
    pub const GETITIMER: usize = 102;
    /// Arm or disarm an interval timer.
    pub const SETITIMER: usize = 103;
    /// Load a kernel module from a buffer.
    pub const INIT_MODULE: usize = 105;
    /// Unload a kernel module.
    pub const DELETE_MODULE: usize = 106;
    /// Set a clock.
    pub const CLOCK_SETTIME: usize = 112;
    /// Read a clock.
    pub const CLOCK_GETTIME: usize = 113;
    /// Read a clock's resolution.
    pub const CLOCK_GETRES: usize = 114;
    /// Sleep against a chosen clock, optionally until an absolute time.
    pub const CLOCK_NANOSLEEP: usize = 115;
    /// Read or control the kernel log buffer.
    pub const SYSLOG: usize = 116;
    /// Set a thread's scheduling parameters.
    pub const SCHED_SETPARAM: usize = 118;
    /// Set a thread's scheduling policy and parameters.
    pub const SCHED_SETSCHEDULER: usize = 119;
    /// Read a thread's scheduling policy.
    pub const SCHED_GETSCHEDULER: usize = 120;
    /// Read a thread's scheduling parameters.
    pub const SCHED_GETPARAM: usize = 121;
    /// Set a thread's processor affinity mask.
    pub const SCHED_SETAFFINITY: usize = 122;
    /// Read a thread's processor affinity mask.
    pub const SCHED_GETAFFINITY: usize = 123;
    /// Yield the processor to another runnable thread.
    pub const SCHED_YIELD: usize = 124;
    /// Report the highest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MAX: usize = 125;
    /// Report the lowest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MIN: usize = 126;
    /// Report a thread's round-robin time slice.
    pub const SCHED_RR_GET_INTERVAL: usize = 127;
    /// Send a signal to a process or process group.
    pub const KILL: usize = 129;
    /// Send a signal to a thread by thread identifier.
    pub const TKILL: usize = 130;
    /// Send a signal to a thread, checked against its thread group.
    pub const TGKILL: usize = 131;
    /// Install or query the alternate signal stack.
    pub const SIGALTSTACK: usize = 132;
    /// Replace the signal mask and wait for a signal.
    pub const RT_SIGSUSPEND: usize = 133;
    /// Install a signal handler.
    pub const RT_SIGACTION: usize = 134;
    /// Change the blocked signal mask.
    pub const RT_SIGPROCMASK: usize = 135;
    /// Report the blocked signals that are pending.
    pub const RT_SIGPENDING: usize = 136;
    /// Wait for one of a set of signals, with a timeout.
    pub const RT_SIGTIMEDWAIT: usize = 137;
    /// Send a signal with caller-supplied `siginfo` to a process.
    pub const RT_SIGQUEUEINFO: usize = 138;
    /// Return from a signal handler, restoring the interrupted context.
    pub const RT_SIGRETURN: usize = 139;
    /// Resume a system call interrupted by a signal handler. The kernel points
    /// a restarted call's number register here; no program issues it itself.
    pub const RESTART_SYSCALL: usize = 128;
    /// Set the nice value of a process, process group or user.
    pub const SETPRIORITY: usize = 140;
    /// Read the nice value of a process, process group or user.
    pub const GETPRIORITY: usize = 141;
    /// Reboot, halt or power off the machine.
    pub const REBOOT: usize = 142;
    /// Set the real and effective group identifiers.
    pub const SETREGID: usize = 143;
    /// Set the group identifier.
    pub const SETGID: usize = 144;
    /// Set the real and effective user identifiers.
    pub const SETREUID: usize = 145;
    /// Set the user identifier.
    pub const SETUID: usize = 146;
    /// Set the real, effective and saved user identifiers.
    pub const SETRESUID: usize = 147;
    /// Read the real, effective and saved user identifiers.
    pub const GETRESUID: usize = 148;
    /// Set the real, effective and saved group identifiers.
    pub const SETRESGID: usize = 149;
    /// Read the real, effective and saved group identifiers.
    pub const GETRESGID: usize = 150;
    /// Set the user identifier used for file access checks.
    pub const SETFSUID: usize = 151;
    /// Set the group identifier used for file access checks.
    pub const SETFSGID: usize = 152;
    /// Report the CPU time used by the process and its children.
    pub const TIMES: usize = 153;
    /// Set a process's process-group identifier.
    pub const SETPGID: usize = 154;
    /// Return a process's process-group identifier.
    pub const GETPGID: usize = 155;
    /// Return a process's session identifier.
    pub const GETSID: usize = 156;
    /// Start a new session, with the caller as its leader.
    pub const SETSID: usize = 157;
    /// Read the supplementary group list.
    pub const GETGROUPS: usize = 158;
    /// Replace the supplementary group list.
    pub const SETGROUPS: usize = 159;
    /// Report kernel name and version.
    pub const UNAME: usize = 160;
    /// Set the host name `uname` reports.
    pub const SETHOSTNAME: usize = 161;
    /// Set the NIS domain name `uname` reports.
    pub const SETDOMAINNAME: usize = 162;
    /// Read a resource limit; superseded by `prlimit64`.
    pub const GETRLIMIT: usize = 163;
    /// Set a resource limit; superseded by `prlimit64`.
    pub const SETRLIMIT: usize = 164;
    /// Report accumulated resource usage.
    pub const GETRUSAGE: usize = 165;
    /// Set the file mode creation mask.
    pub const UMASK: usize = 166;
    /// Operate on per-process control settings, such as the thread name.
    pub const PRCTL: usize = 167;
    /// Report the processor and NUMA node the caller is running on.
    pub const GETCPU: usize = 168;
    /// Read the wall clock, the obsolete predecessor of `clock_gettime`.
    pub const GETTIMEOFDAY: usize = 169;
    /// Set the wall clock, the obsolete predecessor of `clock_settime`.
    pub const SETTIMEOFDAY: usize = 170;
    /// Read or tune the system clock's discipline.
    pub const ADJTIMEX: usize = 171;
    /// Return the calling process's identifier.
    pub const GETPID: usize = 172;
    /// Return the parent process's identifier.
    pub const GETPPID: usize = 173;
    /// Return the real user identifier.
    pub const GETUID: usize = 174;
    /// Return the effective user identifier.
    pub const GETEUID: usize = 175;
    /// Return the real group identifier.
    pub const GETGID: usize = 176;
    /// Return the effective group identifier.
    pub const GETEGID: usize = 177;
    /// Return the calling thread's identifier.
    pub const GETTID: usize = 178;
    /// Report system-wide memory and load statistics.
    pub const SYSINFO: usize = 179;
    /// Create or look up a System V message queue.
    pub const MSGGET: usize = 186;
    /// Query or control a System V message queue.
    pub const MSGCTL: usize = 187;
    /// Receive a message from a System V message queue.
    pub const MSGRCV: usize = 188;
    /// Send a message to a System V message queue.
    pub const MSGSND: usize = 189;
    /// Create or look up a System V semaphore set.
    pub const SEMGET: usize = 190;
    /// Query or control a System V semaphore set.
    pub const SEMCTL: usize = 191;
    /// Operate on System V semaphores, giving up after a timeout.
    pub const SEMTIMEDOP: usize = 192;
    /// Operate on the semaphores in a System V set.
    pub const SEMOP: usize = 193;
    /// Create or look up a System V shared memory segment.
    pub const SHMGET: usize = 194;
    /// Query or control a System V shared memory segment.
    pub const SHMCTL: usize = 195;
    /// Attach a System V shared memory segment.
    pub const SHMAT: usize = 196;
    /// Detach a System V shared memory segment.
    pub const SHMDT: usize = 197;
    /// Create a socket.
    pub const SOCKET: usize = 198;
    /// Create a pair of connected sockets.
    pub const SOCKETPAIR: usize = 199;
    /// Bind a socket to a local address.
    pub const BIND: usize = 200;
    /// Mark a socket as accepting connections.
    pub const LISTEN: usize = 201;
    /// Accept a connection on a listening socket.
    pub const ACCEPT: usize = 202;
    /// Connect a socket to an address.
    pub const CONNECT: usize = 203;
    /// Read a socket's local address.
    pub const GETSOCKNAME: usize = 204;
    /// Read the address of a socket's peer.
    pub const GETPEERNAME: usize = 205;
    /// Send a message on a socket, optionally to an address.
    pub const SENDTO: usize = 206;
    /// Receive a message from a socket, with its source address.
    pub const RECVFROM: usize = 207;
    /// Set a socket option.
    pub const SETSOCKOPT: usize = 208;
    /// Read a socket option.
    pub const GETSOCKOPT: usize = 209;
    /// Shut down part or all of a full-duplex connection.
    pub const SHUTDOWN: usize = 210;
    /// Send a message with ancillary data on a socket.
    pub const SENDMSG: usize = 211;
    /// Receive a message with ancillary data from a socket.
    pub const RECVMSG: usize = 212;
    /// Ask for a range of a file to be read into the page cache ahead of use.
    pub const READAHEAD: usize = 213;
    /// Move the program break, the classic heap boundary.
    pub const BRK: usize = 214;
    /// Remove a mapping.
    pub const MUNMAP: usize = 215;
    /// Resize, and possibly move, an existing mapping.
    pub const MREMAP: usize = 216;
    /// Create a process or thread; the primitive behind `fork` and `pthread`.
    pub const CLONE: usize = 220;
    /// Replace the current process image with a program.
    pub const EXECVE: usize = 221;
    /// Map files or anonymous memory into the address space.
    pub const MMAP: usize = 222;
    /// Start swapping to a file or device.
    pub const SWAPON: usize = 224;
    /// Stop swapping to a file or device.
    pub const SWAPOFF: usize = 225;
    /// Change the protection of a mapping.
    pub const MPROTECT: usize = 226;
    /// Flush a file-backed mapping to its file.
    pub const MSYNC: usize = 227;
    /// Report which pages of a mapping are resident.
    pub const MINCORE: usize = 232;
    /// Advise the kernel about future use of a memory range.
    pub const MADVISE: usize = 233;
    /// Accept a connection, with flags for the new descriptor.
    pub const ACCEPT4: usize = 242;
    /// Wait for a child to change state, reporting resource usage.
    pub const WAIT4: usize = 260;
    /// Read and set a resource limit of any process in one call.
    pub const PRLIMIT64: usize = 261;
    /// Encode a file handle for a path; opening one back is `open_by_handle_at`.
    pub const NAME_TO_HANDLE_AT: usize = 264;
    /// Read or tune a chosen clock's discipline.
    pub const CLOCK_ADJTIME: usize = 266;
    /// Move the calling thread into an existing namespace.
    pub const SETNS: usize = 268;
    /// Load a kernel module from a file descriptor.
    pub const FINIT_MODULE: usize = 273;
    /// Restrict the system calls a thread may make: `seccomp(2)`.
    pub const SECCOMP: usize = 277;
    /// Fill a buffer with random bytes.
    pub const GETRANDOM: usize = 278;
    /// Create an anonymous file living in memory.
    pub const MEMFD_CREATE: usize = 279;
    /// Execute a program named by a file descriptor.
    pub const EXECVEAT: usize = 281;
    /// Issue a process-wide memory barrier.
    pub const MEMBARRIER: usize = 283;
    /// Copy a range of one regular file into another inside the kernel.
    pub const COPY_FILE_RANGE: usize = 285;
    /// Stat a file with an explicit field mask and 64-bit timestamps.
    pub const STATX: usize = 291;
    /// Register a restartable sequence area.
    pub const RSEQ: usize = 293;
    /// Send a signal to the process a pidfd refers to.
    pub const PIDFD_SEND_SIGNAL: usize = 424;
    /// Make a pidfd: a descriptor for a process, readable once it has ended.
    pub const PIDFD_OPEN: usize = 434;
    /// Create a process or thread from a versioned argument structure.
    ///
    /// Numbers assigned after the generic table was frozen are the same on both
    /// architectures, which is why this matches the x86-64 value.
    pub const CLONE3: usize = 435;
    /// Open a file from a versioned argument structure.
    pub const OPENAT2: usize = 437;
    /// Check accessibility with flags, the form musl now prefers.
    pub const FACCESSAT2: usize = 439;
    /// Wait on an epoll set with a signal mask and a nanosecond timeout.
    pub const EPOLL_PWAIT2: usize = 441;

    // -- Filesystem control and extended attributes, from the generic `unistd.h` --

    /// Set an extended attribute by path, following symbolic links.
    pub const SETXATTR: usize = 5;
    /// Set an extended attribute by path, on a symbolic link itself.
    pub const LSETXATTR: usize = 6;
    /// Set an extended attribute of an open file.
    pub const FSETXATTR: usize = 7;
    /// Read an extended attribute by path, following symbolic links.
    pub const GETXATTR: usize = 8;
    /// Read an extended attribute by path, of a symbolic link itself.
    pub const LGETXATTR: usize = 9;
    /// Read an extended attribute of an open file.
    pub const FGETXATTR: usize = 10;
    /// List extended attribute names by path, following symbolic links.
    pub const LISTXATTR: usize = 11;
    /// List extended attribute names by path, of a symbolic link itself.
    pub const LLISTXATTR: usize = 12;
    /// List extended attribute names of an open file.
    pub const FLISTXATTR: usize = 13;
    /// Remove an extended attribute by path, following symbolic links.
    pub const REMOVEXATTR: usize = 14;
    /// Remove an extended attribute by path, from a symbolic link itself.
    pub const LREMOVEXATTR: usize = 15;
    /// Remove an extended attribute of an open file.
    pub const FREMOVEXATTR: usize = 16;
    /// Make another mount the root, and move the old root beneath it.
    pub const PIVOT_ROOT: usize = 41;
    /// Reserve or release storage for a range of an open file.
    pub const FALLOCATE: usize = 47;
    /// Change the calling process's root directory.
    pub const CHROOT: usize = 51;
    /// Flush the file system holding an open file.
    pub const SYNCFS: usize = 267;
}

/// System call numbers for 32-bit ARMv7-A, the EABI table.
///
/// From `arch/arm/include/uapi/asm/unistd-common.h` with `__NR_SYSCALL_BASE`
/// at zero, which is what EABI sets it to; the OABI table this replaced based
/// every number at `0x900000` and is not a target here.
///
/// # Why so many names end in `64`
///
/// This is the only 32-bit architecture Ferrix targets, and the difference is
/// not cosmetic. A register cannot carry a file offset, a file size or a
/// `time_t`, so the calls that pass one exist twice: an original that has
/// outgrown its arguments, and a replacement that splits them across two
/// registers or points at a wider structure. musl calls the replacement
/// every time, so those are the numbers a real binary arrives with.
pub mod arm {
    /// Resume a system call interrupted by a signal handler. The kernel points
    /// a restarted call's number register (`r7`) here; no program issues it
    /// itself. `__NR_SYSCALL_BASE` is zero on the EABI, so this is bare zero.
    pub const RESTART_SYSCALL: usize = 0;
    /// Terminate the calling thread.
    pub const EXIT: usize = 1;
    /// Create a child process sharing nothing.
    pub const FORK: usize = 2;
    /// Read bytes from a file descriptor.
    pub const READ: usize = 3;
    /// Write bytes to a file descriptor.
    pub const WRITE: usize = 4;
    /// Open a file by path.
    pub const OPEN: usize = 5;
    /// Close a file descriptor.
    pub const CLOSE: usize = 6;
    /// Create a file, or truncate it, and open it for writing.
    pub const CREAT: usize = 8;
    /// Create a hard link.
    pub const LINK: usize = 9;
    /// Remove a directory entry.
    pub const UNLINK: usize = 10;
    /// Replace the current process image with a program.
    pub const EXECVE: usize = 11;
    /// Change the working directory by path.
    pub const CHDIR: usize = 12;
    /// Create a file, device node, pipe or socket name by path.
    pub const MKNOD: usize = 14;
    /// Change a file's mode by path.
    pub const CHMOD: usize = 15;
    /// Reposition a file descriptor's offset.
    pub const LSEEK: usize = 19;
    /// Return the calling process's identifier.
    pub const GETPID: usize = 20;
    /// Attach a file system.
    pub const MOUNT: usize = 21;
    /// Sleep until a signal arrives.
    pub const PAUSE: usize = 29;
    /// Check a path's accessibility for the real user.
    pub const ACCESS: usize = 33;
    /// Flush all file systems.
    pub const SYNC: usize = 36;
    /// Send a signal to a process or process group.
    pub const KILL: usize = 37;
    /// Rename a file.
    pub const RENAME: usize = 38;
    /// Create a directory.
    pub const MKDIR: usize = 39;
    /// Remove an empty directory.
    pub const RMDIR: usize = 40;
    /// Duplicate a file descriptor onto the lowest free number.
    pub const DUP: usize = 41;
    /// Create a pipe, returning two file descriptors.
    pub const PIPE: usize = 42;
    /// Report the CPU time used by the process and its children.
    pub const TIMES: usize = 43;
    /// Move the program break, the classic heap boundary.
    pub const BRK: usize = 45;
    /// Turn process accounting on or off.
    pub const ACCT: usize = 51;
    /// Detach a file system.
    pub const UMOUNT2: usize = 52;
    /// Device-specific control operation on a file descriptor.
    pub const IOCTL: usize = 54;
    /// Manipulate a file descriptor's flags and locks.
    pub const FCNTL: usize = 55;
    /// Set a process's process-group identifier.
    pub const SETPGID: usize = 57;
    /// Set the file mode creation mask.
    pub const UMASK: usize = 60;
    /// Duplicate a file descriptor onto a chosen number.
    pub const DUP2: usize = 63;
    /// Return the parent process's identifier.
    pub const GETPPID: usize = 64;
    /// Return the calling process's process-group identifier.
    pub const GETPGRP: usize = 65;
    /// Start a new session, with the caller as its leader.
    pub const SETSID: usize = 66;
    /// Set the host name `uname` reports.
    pub const SETHOSTNAME: usize = 74;
    /// Set a resource limit; superseded by `prlimit64`.
    pub const SETRLIMIT: usize = 75;
    /// Report accumulated resource usage.
    pub const GETRUSAGE: usize = 77;
    /// Read the wall clock, the obsolete predecessor of `clock_gettime`.
    pub const GETTIMEOFDAY: usize = 78;
    /// Set the wall clock, the obsolete predecessor of `clock_settime`.
    pub const SETTIMEOFDAY: usize = 79;
    /// Create a symbolic link.
    pub const SYMLINK: usize = 83;
    /// Read a symbolic link's target.
    pub const READLINK: usize = 85;
    /// Start swapping to a file or device.
    pub const SWAPON: usize = 87;
    /// Reboot, halt or power off the machine.
    pub const REBOOT: usize = 88;
    /// Remove a mapping.
    pub const MUNMAP: usize = 91;
    /// Set a file's length by path.
    pub const TRUNCATE: usize = 92;
    /// Set an open file's length.
    pub const FTRUNCATE: usize = 93;
    /// Change an open file's mode.
    pub const FCHMOD: usize = 94;
    /// Read the nice value of a process, process group or user.
    pub const GETPRIORITY: usize = 96;
    /// Set the nice value of a process, process group or user.
    pub const SETPRIORITY: usize = 97;
    /// Read or control the kernel log buffer.
    pub const SYSLOG: usize = 103;
    /// Arm or disarm an interval timer.
    pub const SETITIMER: usize = 104;
    /// Read an interval timer.
    pub const GETITIMER: usize = 105;
    /// Simulate a hangup on the calling process's terminal.
    pub const VHANGUP: usize = 111;
    /// Wait for a child to change state, reporting resource usage.
    pub const WAIT4: usize = 114;
    /// Stop swapping to a file or device.
    pub const SWAPOFF: usize = 115;
    /// Report system-wide memory and load statistics.
    pub const SYSINFO: usize = 116;
    /// Flush a file's data and metadata to storage.
    pub const FSYNC: usize = 118;
    /// Return from a handler entered without `SA_SIGINFO`, through the frame
    /// that has no `siginfo` in front of it.
    pub const SIGRETURN: usize = 119;
    /// Create a process or thread; the primitive behind `fork` and `pthread`.
    pub const CLONE: usize = 120;
    /// Set the NIS domain name `uname` reports.
    pub const SETDOMAINNAME: usize = 121;
    /// Report kernel name and version.
    pub const UNAME: usize = 122;
    /// Read or tune the system clock's discipline.
    pub const ADJTIMEX: usize = 124;
    /// Change the protection of a mapping.
    pub const MPROTECT: usize = 125;
    /// Load a kernel module from a buffer.
    pub const INIT_MODULE: usize = 128;
    /// Unload a kernel module.
    pub const DELETE_MODULE: usize = 129;
    /// Return a process's process-group identifier.
    pub const GETPGID: usize = 132;
    /// Change the working directory to an open directory.
    pub const FCHDIR: usize = 133;
    /// Read or set the process's execution domain.
    pub const PERSONALITY: usize = 136;
    /// Reposition a file descriptor's offset, with the offset split across
    /// two registers and the result written through a pointer. ARMv7-A only,
    /// because a 32-bit register cannot carry a 64-bit offset and the return
    /// register cannot carry one back.
    pub const LLSEEK: usize = 140;
    /// Wait for readiness on sets of file descriptors, with a `timeval` timeout.
    pub const NEWSELECT: usize = 142;
    /// Take or release an advisory lock on a whole open file.
    pub const FLOCK: usize = 143;
    /// Flush a file-backed mapping to its file.
    pub const MSYNC: usize = 144;
    /// Read into several buffers in one call.
    pub const READV: usize = 145;
    /// Write from several buffers in one call.
    pub const WRITEV: usize = 146;
    /// Return a process's session identifier.
    pub const GETSID: usize = 147;
    /// Flush a file's data, and only the metadata needed to read it back.
    pub const FDATASYNC: usize = 148;
    /// Set a thread's scheduling parameters.
    pub const SCHED_SETPARAM: usize = 154;
    /// Read a thread's scheduling parameters.
    pub const SCHED_GETPARAM: usize = 155;
    /// Set a thread's scheduling policy and parameters.
    pub const SCHED_SETSCHEDULER: usize = 156;
    /// Read a thread's scheduling policy.
    pub const SCHED_GETSCHEDULER: usize = 157;
    /// Yield the processor to another runnable thread.
    pub const SCHED_YIELD: usize = 158;
    /// Report the highest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MAX: usize = 159;
    /// Report the lowest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MIN: usize = 160;
    /// Report a thread's round-robin time slice.
    pub const SCHED_RR_GET_INTERVAL: usize = 161;
    /// Sleep for a duration, resumable after a signal.
    pub const NANOSLEEP: usize = 162;
    /// Resize, and possibly move, an existing mapping.
    pub const MREMAP: usize = 163;
    /// Wait for events on a set of file descriptors.
    pub const POLL: usize = 168;
    /// Operate on per-process control settings, such as the thread name.
    pub const PRCTL: usize = 172;
    /// Return from a signal handler, restoring the interrupted context.
    pub const RT_SIGRETURN: usize = 173;
    /// Install a signal handler.
    pub const RT_SIGACTION: usize = 174;
    /// Change the blocked signal mask.
    pub const RT_SIGPROCMASK: usize = 175;
    /// Report the blocked signals that are pending.
    pub const RT_SIGPENDING: usize = 176;
    /// Wait for one of a set of signals, with a timeout.
    pub const RT_SIGTIMEDWAIT: usize = 177;
    /// Send a signal with caller-supplied `siginfo` to a process.
    pub const RT_SIGQUEUEINFO: usize = 178;
    /// Replace the signal mask and wait for a signal.
    pub const RT_SIGSUSPEND: usize = 179;
    /// Read at an explicit offset, leaving the file position alone.
    pub const PREAD64: usize = 180;
    /// Write at an explicit offset, leaving the file position alone.
    pub const PWRITE64: usize = 181;
    /// Read the current working directory into a buffer.
    pub const GETCWD: usize = 183;
    /// Read a thread's capability sets.
    pub const CAPGET: usize = 184;
    /// Set a thread's capability sets.
    pub const CAPSET: usize = 185;
    /// Install or query the alternate signal stack.
    pub const SIGALTSTACK: usize = 186;
    /// Create a child sharing the address space, suspending the parent.
    pub const VFORK: usize = 190;
    /// Read a resource limit; superseded by `prlimit64`.
    pub const UGETRLIMIT: usize = 191;
    /// Map files or anonymous memory, with the file offset counted in
    /// 4096-byte units. ARMv7-A only, and *not* interchangeable with
    /// [`super::Syscall::Mmap`]: the sixth argument must be multiplied by 4096
    /// before use, and a handler that forgets maps the wrong part of the
    /// file.
    pub const MMAP2: usize = 192;
    /// Set a file's length by path, with the length split across two
    /// registers. ARMv7-A only.
    pub const TRUNCATE64: usize = 193;
    /// Set an open file's length, with the length split across two registers.
    /// ARMv7-A only.
    pub const FTRUNCATE64: usize = 194;
    /// Stat a file by path, following symlinks, into `struct stat64`. ARMv7-A
    /// only: the 32-bit `stat` it replaced cannot express a file larger than
    /// 2 GiB.
    pub const STAT64: usize = 195;
    /// Stat a file by path without following a final symlink, into `struct
    /// stat64`. ARMv7-A only.
    pub const LSTAT64: usize = 196;
    /// Stat an open file descriptor into `struct stat64`. ARMv7-A only.
    pub const FSTAT64: usize = 197;
    /// Change a file's owner by path without following a final symlink, with
    /// 32-bit identifiers. The 16-bit `lchown` at 16 is not carried.
    pub const LCHOWN32: usize = 198;
    /// Return the real user identifier.
    pub const GETUID32: usize = 199;
    /// Return the real group identifier.
    pub const GETGID32: usize = 200;
    /// Return the effective user identifier.
    pub const GETEUID32: usize = 201;
    /// Return the effective group identifier.
    pub const GETEGID32: usize = 202;
    /// Set the real and effective user identifiers.
    pub const SETREUID32: usize = 203;
    /// Set the real and effective group identifiers.
    pub const SETREGID32: usize = 204;
    /// Read the supplementary group list.
    pub const GETGROUPS32: usize = 205;
    /// Replace the supplementary group list.
    pub const SETGROUPS32: usize = 206;
    /// Change an open file's owner.
    pub const FCHOWN32: usize = 207;
    /// Set the real, effective and saved user identifiers.
    pub const SETRESUID32: usize = 208;
    /// Read the real, effective and saved user identifiers.
    pub const GETRESUID32: usize = 209;
    /// Set the real, effective and saved group identifiers.
    pub const SETRESGID32: usize = 210;
    /// Read the real, effective and saved group identifiers.
    pub const GETRESGID32: usize = 211;
    /// Change a file's owner by path.
    pub const CHOWN32: usize = 212;
    /// Set the user identifier.
    pub const SETUID32: usize = 213;
    /// Set the group identifier.
    pub const SETGID32: usize = 214;
    /// Set the user identifier used for file access checks.
    pub const SETFSUID32: usize = 215;
    /// Set the group identifier used for file access checks.
    pub const SETFSGID32: usize = 216;
    /// Read directory entries in the 64-bit layout.
    pub const GETDENTS64: usize = 217;
    /// Report which pages of a mapping are resident.
    pub const MINCORE: usize = 219;
    /// Advise the kernel about future use of a memory range.
    pub const MADVISE: usize = 220;
    /// Manipulate a file descriptor's flags and locks, with 64-bit `flock64`
    /// structures. ARMv7-A only; musl calls it for every `fcntl`, not only
    /// the locking commands.
    pub const FCNTL64: usize = 221;
    /// Return the calling thread's identifier.
    pub const GETTID: usize = 224;
    /// Ask for a range of a file to be read into the page cache ahead of use.
    pub const READAHEAD: usize = 225;
    /// Send a signal to a thread by thread identifier.
    pub const TKILL: usize = 238;
    /// Copy data between two file descriptors, with a 64-bit offset argument.
    /// ARMv7-A's form of [`super::Syscall::Sendfile`].
    pub const SENDFILE64: usize = 239;
    /// Wait on, or wake, a futex; the primitive under every musl lock.
    pub const FUTEX: usize = 240;
    /// Set a thread's processor affinity mask.
    pub const SCHED_SETAFFINITY: usize = 241;
    /// Read a thread's processor affinity mask.
    pub const SCHED_GETAFFINITY: usize = 242;
    /// Terminate every thread in the process.
    pub const EXIT_GROUP: usize = 248;
    /// Create an epoll set, the size a hint that is ignored.
    pub const EPOLL_CREATE: usize = 250;
    /// Add, modify or remove a file descriptor in an epoll set.
    pub const EPOLL_CTL: usize = 251;
    /// Wait for events on an epoll set.
    pub const EPOLL_WAIT: usize = 252;
    /// Register the address cleared and woken on thread exit.
    pub const SET_TID_ADDRESS: usize = 256;
    /// Set a clock.
    pub const CLOCK_SETTIME: usize = 262;
    /// Read a clock.
    pub const CLOCK_GETTIME: usize = 263;
    /// Read a clock's resolution into a 32-bit `timespec`.
    pub const CLOCK_GETRES: usize = 264;
    /// Sleep against a chosen clock, optionally until an absolute time.
    pub const CLOCK_NANOSLEEP: usize = 265;
    /// Report file system statistics by path, into `struct statfs64`. ARMv7-A
    /// only, and it takes the structure's size as an explicit second
    /// argument, which [`super::Syscall::Statfs`] does not.
    pub const STATFS64: usize = 266;
    /// Report file system statistics for an open file, into `struct
    /// statfs64`. ARMv7-A only, with the same explicit size argument as
    /// [`super::Syscall::Statfs64`].
    pub const FSTATFS64: usize = 267;
    /// Send a signal to a thread, checked against its thread group.
    pub const TGKILL: usize = 268;
    /// Wait for a child to change state, without necessarily reaping it.
    pub const WAITID: usize = 280;
    /// Create a socket.
    pub const SOCKET: usize = 281;
    /// Bind a socket to a local address.
    pub const BIND: usize = 282;
    /// Connect a socket to an address.
    pub const CONNECT: usize = 283;
    /// Mark a socket as accepting connections.
    pub const LISTEN: usize = 284;
    /// Accept a connection on a listening socket.
    pub const ACCEPT: usize = 285;
    /// Read a socket's local address.
    pub const GETSOCKNAME: usize = 286;
    /// Read the address of a socket's peer.
    pub const GETPEERNAME: usize = 287;
    /// Create a pair of connected sockets.
    pub const SOCKETPAIR: usize = 288;
    /// Send a message on a socket, optionally to an address.
    pub const SENDTO: usize = 290;
    /// Receive a message from a socket, with its source address.
    pub const RECVFROM: usize = 292;
    /// Shut down part or all of a full-duplex connection.
    pub const SHUTDOWN: usize = 293;
    /// Set a socket option.
    pub const SETSOCKOPT: usize = 294;
    /// Read a socket option.
    pub const GETSOCKOPT: usize = 295;
    /// Send a message with ancillary data on a socket.
    pub const SENDMSG: usize = 296;
    /// Receive a message with ancillary data from a socket.
    pub const RECVMSG: usize = 297;
    /// Operate on the semaphores in a System V set.
    pub const SEMOP: usize = 298;
    /// Create or look up a System V semaphore set.
    pub const SEMGET: usize = 299;
    /// Query or control a System V semaphore set.
    pub const SEMCTL: usize = 300;
    /// Send a message to a System V message queue.
    pub const MSGSND: usize = 301;
    /// Receive a message from a System V message queue.
    pub const MSGRCV: usize = 302;
    /// Create or look up a System V message queue.
    pub const MSGGET: usize = 303;
    /// Query or control a System V message queue.
    pub const MSGCTL: usize = 304;
    /// Attach a System V shared memory segment.
    pub const SHMAT: usize = 305;
    /// Detach a System V shared memory segment.
    pub const SHMDT: usize = 306;
    /// Create or look up a System V shared memory segment.
    pub const SHMGET: usize = 307;
    /// Query or control a System V shared memory segment.
    pub const SHMCTL: usize = 308;
    /// Operate on System V semaphores, giving up after a timeout.
    pub const SEMTIMEDOP: usize = 312;
    /// Set a process's I/O scheduling class and priority.
    pub const IOPRIO_SET: usize = 314;
    /// Read a process's I/O scheduling class and priority.
    pub const IOPRIO_GET: usize = 315;
    /// Make an inotify instance, with no flags.
    pub const INOTIFY_INIT: usize = 316;
    /// Watch a path on an inotify instance.
    pub const INOTIFY_ADD_WATCH: usize = 317;
    /// Remove a watch from an inotify instance.
    pub const INOTIFY_RM_WATCH: usize = 318;
    /// Open a file relative to a directory file descriptor.
    pub const OPENAT: usize = 322;
    /// Create a directory relative to a directory file descriptor.
    pub const MKDIRAT: usize = 323;
    /// Create a file, device node, pipe or socket name relative to a
    /// directory file descriptor.
    pub const MKNODAT: usize = 324;
    /// Change a file's owner relative to a directory file descriptor.
    pub const FCHOWNAT: usize = 325;
    /// Stat a file relative to a directory file descriptor, into `struct
    /// stat64`. ARMv7-A's form of [`super::Syscall::Newfstatat`], which it cannot
    /// share because the structure written back has a different layout.
    pub const FSTATAT64: usize = 327;
    /// Remove a directory entry relative to a directory file descriptor.
    pub const UNLINKAT: usize = 328;
    /// Rename relative to directory file descriptors.
    pub const RENAMEAT: usize = 329;
    /// Create a hard link relative to directory file descriptors.
    pub const LINKAT: usize = 330;
    /// Create a symbolic link relative to a directory file descriptor.
    pub const SYMLINKAT: usize = 331;
    /// Read a symbolic link relative to a directory file descriptor.
    pub const READLINKAT: usize = 332;
    /// Change a file's mode relative to a directory file descriptor.
    pub const FCHMODAT: usize = 333;
    /// Check accessibility relative to a directory file descriptor.
    pub const FACCESSAT: usize = 334;
    /// Wait for readiness on descriptor sets, with a signal mask and a `timespec`.
    pub const PSELECT6: usize = 335;
    /// Poll with a signal mask and a `timespec` timeout.
    pub const PPOLL: usize = 336;
    /// Detach parts of the calling process's shared execution context.
    pub const UNSHARE: usize = 337;
    /// Register the robust futex list, walked when a thread dies holding a
    /// lock.
    pub const SET_ROBUST_LIST: usize = 338;
    /// Read a thread's robust futex list.
    pub const GET_ROBUST_LIST: usize = 339;
    /// Move bytes between a pipe and another descriptor inside the kernel.
    pub const SPLICE: usize = 340;
    /// Report the processor and NUMA node the caller is running on.
    pub const GETCPU: usize = 345;
    /// Wait on an epoll set with a signal mask.
    pub const EPOLL_PWAIT: usize = 346;
    /// Set a file's times from two `timespec`s of two `long`s -- 32 bits each
    /// here. [`UTIMENSAT_TIME64`] is the form a time64 musl calls.
    pub const UTIMENSAT: usize = 348;
    /// Create a signalfd, or change one's mask, with no flags.
    pub const SIGNALFD: usize = 349;
    /// Create a timer that expires into a descriptor.
    pub const TIMERFD_CREATE: usize = 350;
    /// Create an eventfd with no flags.
    pub const EVENTFD: usize = 351;
    /// Arm or disarm a timerfd, from `itimerspec`s of two 32-bit `timespec`s.
    /// [`TIMERFD_SETTIME64`] is the form a time64 musl calls.
    pub const TIMERFD_SETTIME: usize = 353;
    /// Read a timerfd's time left and interval into 32-bit `timespec`s.
    /// [`TIMERFD_GETTIME64`] is the form a time64 musl calls.
    pub const TIMERFD_GETTIME: usize = 354;
    /// Create a signalfd, or change one's mask, with flags.
    pub const SIGNALFD4: usize = 355;
    /// Create an eventfd with flags.
    pub const EVENTFD2: usize = 356;
    /// Create an epoll set with flags.
    pub const EPOLL_CREATE1: usize = 357;
    /// Duplicate a file descriptor onto a chosen number with flags.
    pub const DUP3: usize = 358;
    /// Create a pipe with flags.
    pub const PIPE2: usize = 359;
    /// Make an inotify instance, with flags.
    pub const INOTIFY_INIT1: usize = 360;
    /// Accept a connection, with flags for the new descriptor.
    pub const ACCEPT4: usize = 366;
    /// Read and set a resource limit of any process in one call.
    pub const PRLIMIT64: usize = 369;
    /// Encode a file handle for a path; opening one back is `open_by_handle_at`.
    pub const NAME_TO_HANDLE_AT: usize = 370;
    /// Read or tune a chosen clock's discipline.
    pub const CLOCK_ADJTIME: usize = 372;
    /// Move the calling thread into an existing namespace.
    pub const SETNS: usize = 375;
    /// Load a kernel module from a file descriptor.
    pub const FINIT_MODULE: usize = 379;
    /// Rename with flags, such as `RENAME_NOREPLACE`.
    pub const RENAMEAT2: usize = 382;
    /// Restrict the system calls a thread may make: `seccomp(2)`.
    pub const SECCOMP: usize = 383;
    /// Fill a buffer with random bytes.
    pub const GETRANDOM: usize = 384;
    /// Create an anonymous file living in memory.
    pub const MEMFD_CREATE: usize = 385;
    /// Execute a program named by a file descriptor.
    pub const EXECVEAT: usize = 387;
    /// Issue a process-wide memory barrier.
    pub const MEMBARRIER: usize = 389;
    /// Copy a range of one regular file into another inside the kernel.
    pub const COPY_FILE_RANGE: usize = 391;
    /// Stat a file with an explicit field mask and 64-bit timestamps.
    pub const STATX: usize = 397;
    /// Register a restartable sequence area.
    pub const RSEQ: usize = 398;
    /// Read a clock into a 64-bit `timespec`. ARMv7-A only. musl has been
    /// time64 since 1.2, so this, not [`super::Syscall::ClockGettime`], is what a
    /// current 32-bit binary calls.
    pub const CLOCK_GETTIME64: usize = 403;
    /// Set a clock from a 64-bit `timespec`. ARMv7-A only.
    pub const CLOCK_SETTIME64: usize = 404;
    /// Read or tune a chosen clock's discipline, with 64-bit time fields.
    /// ARMv7-A only.
    pub const CLOCK_ADJTIME64: usize = 405;
    /// Read a clock's resolution into a 64-bit `timespec`. ARMv7-A only.
    pub const CLOCK_GETRES_TIME64: usize = 406;
    /// Sleep against a chosen clock, with 64-bit `timespec` arguments.
    /// ARMv7-A only.
    pub const CLOCK_NANOSLEEP_TIME64: usize = 407;
    /// Read a timerfd's time left and interval into 64-bit `timespec`s.
    /// ARMv7-A only.
    pub const TIMERFD_GETTIME64: usize = 410;
    /// Arm or disarm a timerfd from 64-bit `timespec`s. ARMv7-A only.
    pub const TIMERFD_SETTIME64: usize = 411;
    /// Set a file's times from two 64-bit `timespec`s. ARMv7-A only.
    pub const UTIMENSAT_TIME64: usize = 412;
    /// Wait for readiness on descriptor sets, with a signal mask and a 64-bit
    /// `timespec`. ARMv7-A only.
    pub const PSELECT6_TIME64: usize = 413;
    /// Poll with a signal mask and a 64-bit `timespec`. The timeout is a pair
    /// of 64-bit fields rather than the 32-bit pair [`PPOLL`] takes.
    pub const PPOLL_TIME64: usize = 414;
    /// Operate on System V semaphores, with a 64-bit `timespec` timeout.
    pub const SEMTIMEDOP_TIME64: usize = 420;
    /// Wait for one of a set of signals, with a 64-bit `timespec` timeout.
    /// ARMv7-A only.
    pub const RT_SIGTIMEDWAIT_TIME64: usize = 421;
    /// Wait on, or wake, a futex, with a 64-bit `timespec` timeout. ARMv7-A
    /// only, and the one a time64 musl's locks actually reach.
    pub const FUTEX_TIME64: usize = 422;
    /// Report a thread's round-robin time slice into a 64-bit `timespec`.
    /// ARMv7-A only.
    pub const SCHED_RR_GET_INTERVAL_TIME64: usize = 423;
    /// Send a signal to the process a pidfd refers to.
    pub const PIDFD_SEND_SIGNAL: usize = 424;
    /// Make a pidfd: a descriptor for a process, readable once it has ended.
    pub const PIDFD_OPEN: usize = 434;
    /// Create a process or thread from a versioned argument structure.
    pub const CLONE3: usize = 435;
    /// Open a file from a versioned argument structure.
    pub const OPENAT2: usize = 437;
    /// Check accessibility with flags, the form musl now prefers.
    pub const FACCESSAT2: usize = 439;
    /// Wait on an epoll set with a signal mask and a nanosecond timeout.
    pub const EPOLL_PWAIT2: usize = 441;

    /// The base of the ARM-private call range, `__ARM_NR_BASE`.
    /// Four registers and two cache operations that no other architecture
    /// needs, parked far above the shared table so that a number added to it
    /// can never collide with one of these.
    pub const ARM_PRIVATE_BASE: usize = 0x000f_0000;

    /// Make a range coherent between the data and instruction caches.
    pub const ARM_CACHEFLUSH: usize = ARM_PRIVATE_BASE + 2;
    /// Set the thread pointer read through `TPIDRURO`.
    pub const ARM_SET_TLS: usize = ARM_PRIVATE_BASE + 5;

    // -- Filesystem control and extended attributes, from `unistd-common.h` --

    /// Set an extended attribute by path, following symbolic links.
    pub const SETXATTR: usize = 226;
    /// Set an extended attribute by path, on a symbolic link itself.
    pub const LSETXATTR: usize = 227;
    /// Set an extended attribute of an open file.
    pub const FSETXATTR: usize = 228;
    /// Read an extended attribute by path, following symbolic links.
    pub const GETXATTR: usize = 229;
    /// Read an extended attribute by path, of a symbolic link itself.
    pub const LGETXATTR: usize = 230;
    /// Read an extended attribute of an open file.
    pub const FGETXATTR: usize = 231;
    /// List extended attribute names by path, following symbolic links.
    pub const LISTXATTR: usize = 232;
    /// List extended attribute names by path, of a symbolic link itself.
    pub const LLISTXATTR: usize = 233;
    /// List extended attribute names of an open file.
    pub const FLISTXATTR: usize = 234;
    /// Remove an extended attribute by path, following symbolic links.
    pub const REMOVEXATTR: usize = 235;
    /// Remove an extended attribute by path, from a symbolic link itself.
    pub const LREMOVEXATTR: usize = 236;
    /// Remove an extended attribute of an open file.
    pub const FREMOVEXATTR: usize = 237;
    /// Make another mount the root, and move the old root beneath it.
    pub const PIVOT_ROOT: usize = 218;
    /// Reserve or release storage for a range of an open file.
    pub const FALLOCATE: usize = 352;
    /// Change the calling process's root directory.
    pub const CHROOT: usize = 61;
    /// Flush the file system holding an open file.
    pub const SYNCFS: usize = 373;
}

/// System call numbers for i386, as a 32-bit program on the x86-64 kernel
/// issues them through `int $0x80`.
///
/// From `arch/x86/entry/syscalls/syscall_32.tbl`, generated from the UAPI
/// header `asm/unistd_32.h` and checked back against it. Only the calls
/// [`from_i386`] translates are here: an i386 call is mapped once its
/// handler has been read for the 32-bit widths it sees (`docs/I386.md`
/// §3.2), and a number with no constant is one nobody has read yet.
pub mod i386 {
    /// Resume a system call interrupted by a signal handler. The kernel points
    /// a restarted call's number register (`eax`) here; no program issues it
    /// itself.
    pub const RESTART_SYSCALL: usize = 0;
    /// Terminate the calling thread.
    pub const EXIT: usize = 1;
    /// Read bytes from a file descriptor.
    pub const READ: usize = 3;
    /// Write bytes to a file descriptor.
    pub const WRITE: usize = 4;
    /// Close a file descriptor.
    pub const CLOSE: usize = 6;
    /// Return the calling process's identifier.
    pub const GETPID: usize = 20;
    /// Duplicate a file descriptor onto the lowest free number.
    pub const DUP: usize = 41;
    /// Duplicate a file descriptor onto a chosen number.
    pub const DUP2: usize = 63;
    /// Return the parent process's identifier.
    pub const GETPPID: usize = 64;
    /// Describe the running kernel: `struct new_utsname`, six 65-byte fields
    /// on every architecture.
    pub const UNAME: usize = 122;
    /// Give up the processor to another runnable task.
    pub const SCHED_YIELD: usize = 158;
    /// Return the real user identifier. The `32` form: i386's number 24 is
    /// the 16-bit one, which is absent for the reason `nr.rs` gives for
    /// ARMv7-A's.
    pub const GETUID32: usize = 199;
    /// Return the real group identifier, the `32` form.
    pub const GETGID32: usize = 200;
    /// Return the effective user identifier, the `32` form.
    pub const GETEUID32: usize = 201;
    /// Return the effective group identifier, the `32` form.
    pub const GETEGID32: usize = 202;
    /// Return the calling thread's identifier.
    pub const GETTID: usize = 224;
    /// Install a thread-local segment descriptor.
    pub const SET_THREAD_AREA: usize = 243;
    /// Read a thread-local segment descriptor back.
    pub const GET_THREAD_AREA: usize = 244;
    /// Terminate every thread in the process.
    pub const EXIT_GROUP: usize = 252;
    /// Duplicate a file descriptor onto a chosen number, with flags.
    pub const DUP3: usize = 330;
    /// Create a child process sharing nothing.
    pub const FORK: usize = 2;
    /// Send a signal to a process or process group.
    pub const KILL: usize = 37;
    /// Wait for a child, with its resource use: a 32-bit `struct rusage`.
    pub const WAIT4: usize = 114;
    /// Return from a handler entered without `SA_SIGINFO`, through `sigframe_ia32`.
    pub const SIGRETURN: usize = 119;
    /// Create a process or thread, `CLONE_BACKWARDS`' order: `tls` before `child_tid`, and `tls` a `struct user_desc`.
    pub const CLONE: usize = 120;
    /// Return from a handler entered with `SA_SIGINFO`, through `rt_sigframe_ia32`.
    pub const RT_SIGRETURN: usize = 173;
    /// Examine or change a signal's action: a 32-bit `struct sigaction`.
    pub const RT_SIGACTION: usize = 174;
    /// Examine or change the blocked mask.
    pub const RT_SIGPROCMASK: usize = 175;
    /// Wait for a signal with a mask in place.
    pub const RT_SIGSUSPEND: usize = 179;
    /// Examine or change the alternate signal stack: a 32-bit `stack_t`.
    pub const SIGALTSTACK: usize = 186;
    /// Create a child sharing memory until it execs or exits.
    pub const VFORK: usize = 190;
    /// Send a signal to one thread.
    pub const TKILL: usize = 238;
    /// Send a signal to one thread of a given process.
    pub const TGKILL: usize = 270;
    /// Open a file by path.
    pub const OPEN: usize = 5;
    /// Create a file, or truncate it, and open it for writing.
    pub const CREAT: usize = 8;
    /// Create a hard link.
    pub const LINK: usize = 9;
    /// Remove a directory entry.
    pub const UNLINK: usize = 10;
    /// Execute a program, reading `argv` and `envp` as arrays of 32-bit
    /// pointers.
    pub const EXECVE: usize = 11;
    /// Change the working directory by path.
    pub const CHDIR: usize = 12;
    /// Create a file, device node, pipe or socket name by path.
    pub const MKNOD: usize = 14;
    /// Change a file's mode by path.
    pub const CHMOD: usize = 15;
    /// Reposition a file descriptor's offset by a 32-bit signed `off_t`.
    pub const LSEEK: usize = 19;
    /// Attach a file system.
    pub const MOUNT: usize = 21;
    /// Check a path's accessibility for the real user.
    pub const ACCESS: usize = 33;
    /// Flush all file systems.
    pub const SYNC: usize = 36;
    /// Rename a file.
    pub const RENAME: usize = 38;
    /// Create a directory.
    pub const MKDIR: usize = 39;
    /// Remove an empty directory.
    pub const RMDIR: usize = 40;
    /// Create a pipe, returning two file descriptors.
    pub const PIPE: usize = 42;
    /// Move the program break, the classic heap boundary.
    pub const BRK: usize = 45;
    /// Detach a file system.
    pub const UMOUNT2: usize = 52;
    /// Device-specific control. Only the requests whose argument reads the same
    /// at both widths reach a handler; see the kernel's `syscall::compat`.
    pub const IOCTL: usize = 54;
    /// Manipulate a file descriptor's flags and locks, with the 16-byte 32-bit
    /// `flock`.
    pub const FCNTL: usize = 55;
    /// Set a process's process-group identifier.
    pub const SETPGID: usize = 57;
    /// Set the file mode creation mask.
    pub const UMASK: usize = 60;
    /// Change the calling process's root directory.
    pub const CHROOT: usize = 61;
    /// Return the calling process's process-group identifier.
    pub const GETPGRP: usize = 65;
    /// Start a new session, with the caller as its leader.
    pub const SETSID: usize = 66;
    /// Set the host name `uname` reports.
    pub const SETHOSTNAME: usize = 74;
    /// Create a symbolic link.
    pub const SYMLINK: usize = 83;
    /// Read a symbolic link's target.
    pub const READLINK: usize = 85;
    /// Remove a mapping.
    pub const MUNMAP: usize = 91;
    /// Set a file's length by path, from a 32-bit signed `off_t`.
    pub const TRUNCATE: usize = 92;
    /// Set an open file's length, from a 32-bit signed `off_t`.
    pub const FTRUNCATE: usize = 93;
    /// Change an open file's mode.
    pub const FCHMOD: usize = 94;
    /// Read the nice value of a process, process group or user.
    pub const GETPRIORITY: usize = 96;
    /// Set the nice value of a process, process group or user.
    pub const SETPRIORITY: usize = 97;
    /// Flush a file's data and metadata to storage.
    pub const FSYNC: usize = 118;
    /// Set the NIS domain name `uname` reports.
    pub const SETDOMAINNAME: usize = 121;
    /// Change the protection of a mapping.
    pub const MPROTECT: usize = 125;
    /// Return a process's process-group identifier.
    pub const GETPGID: usize = 132;
    /// Change the working directory to an open directory.
    pub const FCHDIR: usize = 133;
    /// Reposition a file descriptor's offset, with the offset split across two
    /// registers and the result written through a pointer: `_llseek`, for the
    /// reason ARMv7-A has it.
    pub const LLSEEK: usize = 140;
    /// Take or release an advisory lock on a whole open file.
    pub const FLOCK: usize = 143;
    /// Flush a file-backed mapping to its file.
    pub const MSYNC: usize = 144;
    /// Scatter-read into a 32-bit `iovec` array.
    pub const READV: usize = 145;
    /// Gather-write from a 32-bit `iovec` array.
    pub const WRITEV: usize = 146;
    /// Return a process's session identifier.
    pub const GETSID: usize = 147;
    /// Flush a file's data, and only the metadata needed to read it back.
    pub const FDATASYNC: usize = 148;
    /// Set a thread's scheduling parameters.
    pub const SCHED_SETPARAM: usize = 154;
    /// Read a thread's scheduling parameters.
    pub const SCHED_GETPARAM: usize = 155;
    /// Set a thread's scheduling policy and parameters.
    pub const SCHED_SETSCHEDULER: usize = 156;
    /// Read a thread's scheduling policy.
    pub const SCHED_GETSCHEDULER: usize = 157;
    /// Report the highest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MAX: usize = 159;
    /// Report the lowest priority a scheduling policy allows.
    pub const SCHED_GET_PRIORITY_MIN: usize = 160;
    /// Resize, and possibly move, an existing mapping.
    pub const MREMAP: usize = 163;
    /// Wait for events on a set of file descriptors.
    pub const POLL: usize = 168;
    /// Operate on per-process control settings, such as the thread name.
    pub const PRCTL: usize = 172;
    /// Read at an offset without moving the file position, the offset in two
    /// registers, low word first, with no alignment to an even register.
    pub const PREAD64: usize = 180;
    /// Write at an offset without moving the file position, the offset in two
    /// registers as `pread64`'s is.
    pub const PWRITE64: usize = 181;
    /// Read the current working directory into a buffer.
    pub const GETCWD: usize = 183;
    /// Read a thread's capability sets.
    pub const CAPGET: usize = 184;
    /// Set a thread's capability sets.
    pub const CAPSET: usize = 185;
    /// Map files or anonymous memory, with the file offset counted in 4096-byte
    /// units. *Not* interchangeable with [`super::Syscall::Mmap`], i386's
    /// number 90, which takes a pointer to its arguments and is not carried.
    pub const MMAP2: usize = 192;
    /// Set a file's length by path, with the length split across two registers,
    /// low word first.
    pub const TRUNCATE64: usize = 193;
    /// Set an open file's length, with the length split across two registers,
    /// low word first.
    pub const FTRUNCATE64: usize = 194;
    /// Change a file's owner by path without following a final symlink, with
    /// 32-bit identifiers. The 16-bit `lchown` at 16 is not carried.
    pub const LCHOWN32: usize = 198;
    /// Set the real and effective user identifiers.
    pub const SETREUID32: usize = 203;
    /// Set the real and effective group identifiers.
    pub const SETREGID32: usize = 204;
    /// Read the supplementary group list.
    pub const GETGROUPS32: usize = 205;
    /// Replace the supplementary group list.
    pub const SETGROUPS32: usize = 206;
    /// Change an open file's owner.
    pub const FCHOWN32: usize = 207;
    /// Set the real, effective and saved user identifiers.
    pub const SETRESUID32: usize = 208;
    /// Read the real, effective and saved user identifiers.
    pub const GETRESUID32: usize = 209;
    /// Set the real, effective and saved group identifiers.
    pub const SETRESGID32: usize = 210;
    /// Read the real, effective and saved group identifiers.
    pub const GETRESGID32: usize = 211;
    /// Change a file's owner by path.
    pub const CHOWN32: usize = 212;
    /// Set the user identifier.
    pub const SETUID32: usize = 213;
    /// Set the group identifier.
    pub const SETGID32: usize = 214;
    /// Set the user identifier used for file access checks.
    pub const SETFSUID32: usize = 215;
    /// Set the group identifier used for file access checks.
    pub const SETFSGID32: usize = 216;
    /// Make another mount the root, and move the old root beneath it.
    pub const PIVOT_ROOT: usize = 217;
    /// Report which pages of a mapping are resident.
    pub const MINCORE: usize = 218;
    /// Advise the kernel about future use of a memory range.
    pub const MADVISE: usize = 219;
    /// Read directory entries in the 64-bit layout.
    pub const GETDENTS64: usize = 220;
    /// Manipulate a file descriptor's flags and locks, with i386's packed
    /// 24-byte `flock64`. musl calls it for every `fcntl`, not only the locking
    /// commands.
    pub const FCNTL64: usize = 221;
    /// Start reading a file into the page cache, the offset in two registers,
    /// low word first.
    pub const READAHEAD: usize = 225;
    /// Set an extended attribute by path, following symbolic links.
    pub const SETXATTR: usize = 226;
    /// Set an extended attribute by path, on a symbolic link itself.
    pub const LSETXATTR: usize = 227;
    /// Set an extended attribute of an open file.
    pub const FSETXATTR: usize = 228;
    /// Read an extended attribute by path, following symbolic links.
    pub const GETXATTR: usize = 229;
    /// Read an extended attribute by path, of a symbolic link itself.
    pub const LGETXATTR: usize = 230;
    /// Read an extended attribute of an open file.
    pub const FGETXATTR: usize = 231;
    /// List extended attribute names by path, following symbolic links.
    pub const LISTXATTR: usize = 232;
    /// List extended attribute names by path, of a symbolic link itself.
    pub const LLISTXATTR: usize = 233;
    /// List extended attribute names of an open file.
    pub const FLISTXATTR: usize = 234;
    /// Remove an extended attribute by path, following symbolic links.
    pub const REMOVEXATTR: usize = 235;
    /// Remove an extended attribute by path, from a symbolic link itself.
    pub const LREMOVEXATTR: usize = 236;
    /// Remove an extended attribute of an open file.
    pub const FREMOVEXATTR: usize = 237;
    /// Copy data between two file descriptors, with a 64-bit offset argument.
    pub const SENDFILE64: usize = 239;
    /// Register the address cleared and woken on thread exit.
    pub const SET_TID_ADDRESS: usize = 258;
    /// Create an epoll set, the size a hint that is ignored.
    pub const EPOLL_CREATE: usize = 254;
    /// Add, modify or remove a file descriptor in an epoll set.
    pub const EPOLL_CTL: usize = 255;
    /// Wait for events on an epoll set.
    pub const EPOLL_WAIT: usize = 256;
    /// Make an inotify instance, with no flags.
    pub const INOTIFY_INIT: usize = 291;
    /// Watch a path on an inotify instance.
    pub const INOTIFY_ADD_WATCH: usize = 292;
    /// Remove a watch from an inotify instance.
    pub const INOTIFY_RM_WATCH: usize = 293;
    /// Open a file relative to a directory file descriptor.
    pub const OPENAT: usize = 295;
    /// Create a directory relative to a directory file descriptor.
    pub const MKDIRAT: usize = 296;
    /// Create a file, device node, pipe or socket name relative to a directory
    /// file descriptor.
    pub const MKNODAT: usize = 297;
    /// Change a file's owner relative to a directory file descriptor.
    pub const FCHOWNAT: usize = 298;
    /// Remove a directory entry relative to a directory file descriptor.
    pub const UNLINKAT: usize = 301;
    /// Rename relative to directory file descriptors.
    pub const RENAMEAT: usize = 302;
    /// Create a hard link relative to directory file descriptors.
    pub const LINKAT: usize = 303;
    /// Create a symbolic link relative to a directory file descriptor.
    pub const SYMLINKAT: usize = 304;
    /// Read a symbolic link relative to a directory file descriptor.
    pub const READLINKAT: usize = 305;
    /// Change a file's mode relative to a directory file descriptor.
    pub const FCHMODAT: usize = 306;
    /// Check accessibility relative to a directory file descriptor.
    pub const FACCESSAT: usize = 307;
    /// Detach parts of the calling process's shared execution context.
    pub const UNSHARE: usize = 310;
    /// Move bytes between a pipe and another descriptor inside the kernel.
    pub const SPLICE: usize = 313;
    /// Report the processor and NUMA node the caller is running on.
    pub const GETCPU: usize = 318;
    /// Wait on an epoll set with a signal mask.
    pub const EPOLL_PWAIT: usize = 319;
    /// Create a signalfd, or change one's mask, with no flags.
    pub const SIGNALFD: usize = 321;
    /// Create a timer that expires into a descriptor.
    pub const TIMERFD_CREATE: usize = 322;
    /// Create an eventfd with no flags.
    pub const EVENTFD: usize = 323;
    /// Allocate or deallocate space in a file, the offset and length each in
    /// two registers, low word first.
    pub const FALLOCATE: usize = 324;
    /// Create a signalfd, or change one's mask, with flags.
    pub const SIGNALFD4: usize = 327;
    /// Create an eventfd with flags.
    pub const EVENTFD2: usize = 328;
    /// Create an epoll set with flags.
    pub const EPOLL_CREATE1: usize = 329;
    /// Create a pipe with flags.
    pub const PIPE2: usize = 331;
    /// Make an inotify instance, with flags.
    pub const INOTIFY_INIT1: usize = 332;
    /// Read and set a resource limit of any process in one call.
    pub const PRLIMIT64: usize = 340;
    /// Encode a file handle for a path; opening one back is `open_by_handle_at`.
    pub const NAME_TO_HANDLE_AT: usize = 341;
    /// Flush the file system holding an open file.
    pub const SYNCFS: usize = 344;
    /// Move the calling thread into an existing namespace.
    pub const SETNS: usize = 346;
    /// Rename with flags, such as `RENAME_NOREPLACE`.
    pub const RENAMEAT2: usize = 353;
    /// Restrict the system calls a thread may make: `seccomp(2)`.
    pub const SECCOMP: usize = 354;
    /// Fill a buffer with random bytes.
    pub const GETRANDOM: usize = 355;
    /// Create an anonymous file living in memory.
    pub const MEMFD_CREATE: usize = 356;
    /// Execute a program relative to a directory file descriptor, reading
    /// `argv` and `envp` as arrays of 32-bit pointers.
    pub const EXECVEAT: usize = 358;
    /// Issue a process-wide memory barrier.
    pub const MEMBARRIER: usize = 375;
    /// Copy a range of one regular file into another inside the kernel.
    pub const COPY_FILE_RANGE: usize = 377;
    /// Stat a file with an explicit field mask and 64-bit timestamps.
    pub const STATX: usize = 383;
    /// Read a clock into a 64-bit `timespec`. musl has been time64 since 1.2,
    /// so this, not [`super::Syscall::ClockGettime`], is what a current 32-bit
    /// binary calls.
    pub const CLOCK_GETTIME64: usize = 403;
    /// Set a clock from a 64-bit `timespec`.
    pub const CLOCK_SETTIME64: usize = 404;
    /// Read a clock's resolution into a 64-bit `timespec`.
    pub const CLOCK_GETRES_TIME64: usize = 406;
    /// Sleep against a chosen clock, with 64-bit `timespec` arguments.
    pub const CLOCK_NANOSLEEP_TIME64: usize = 407;
    /// Read a timerfd's time left and interval into 64-bit `timespec`s.
    pub const TIMERFD_GETTIME64: usize = 410;
    /// Arm or disarm a timerfd from 64-bit `timespec`s.
    pub const TIMERFD_SETTIME64: usize = 411;
    /// Set a file's times from two 64-bit `timespec`s.
    pub const UTIMENSAT_TIME64: usize = 412;
    /// Poll with a signal mask and a 64-bit `timespec`. The 32-bit `ppoll` at
    /// 309 is not carried: its timeout is two `long`s.
    pub const PPOLL_TIME64: usize = 414;
    /// Wait on, or wake, a futex, with a 64-bit `timespec` timeout. and the one
    /// a time64 musl's locks actually reach.
    pub const FUTEX_TIME64: usize = 422;
    /// Every System V IPC operation, as a call number (`SEMOP`, `SEMGET`,
    /// `SEMCTL`, `SEMTIMEDOP`, the message and shared-memory ones) with the
    /// `IPC_64` version in its upper half, and five arguments: what glibc on
    /// i386 calls for the semaphores, and musl too, having no direct numbers.
    pub const IPC: usize = 117;
    /// Create or look up a System V semaphore set: the direct number Linux
    /// 5.1 gave i386.
    pub const SEMGET: usize = 393;
    /// Query or control a System V semaphore set, directly. The `cmd` carries
    /// `IPC_64` as it does through [`IPC`].
    pub const SEMCTL: usize = 394;
    /// Operate on System V semaphores with a 64-bit `timespec` timeout. i386
    /// has no direct `semop` or 32-bit `semtimedop`; those go through [`IPC`].
    pub const SEMTIMEDOP_TIME64: usize = 420;
    /// Create or look up a System V shared memory segment, directly: the
    /// number Linux 5.1 gave i386.
    pub const SHMGET: usize = 395;
    /// Query or control a System V shared memory segment, directly. The
    /// `cmd` carries `IPC_64` as it does through [`IPC`].
    pub const SHMCTL: usize = 396;
    /// Attach a System V shared memory segment, directly; the address is the
    /// answer, where [`IPC`]'s `SHMAT` stores it through a pointer.
    pub const SHMAT: usize = 397;
    /// Detach a System V shared memory segment, directly.
    pub const SHMDT: usize = 398;
    /// Report the blocked signals that are pending.
    pub const RT_SIGPENDING: usize = 176;
    /// Sleep until a signal arrives.
    pub const PAUSE: usize = 29;
    /// Make a pidfd: a descriptor for a process, readable once it has ended.
    pub const PIDFD_OPEN: usize = 434;
    /// Open a file from a versioned argument structure.
    pub const OPENAT2: usize = 437;
    /// Check accessibility with flags, the form musl now prefers.
    pub const FACCESSAT2: usize = 439;
    /// Wait on an epoll set with a signal mask and a nanosecond timeout.
    pub const EPOLL_PWAIT2: usize = 441;
    /// Report a filesystem's usage by path into the packed 84-byte
    /// `struct statfs64`, which is ARMv7-A's too.
    pub const STATFS64: usize = 268;
    /// Report the usage of an open file's filesystem, as [`STATFS64`].
    pub const FSTATFS64: usize = 269;
    /// Report uptime, load and memory into the 64-byte 32-bit `struct
    /// sysinfo`.
    pub const SYSINFO: usize = 116;
    /// Restart, halt or power off the machine.
    pub const REBOOT: usize = 88;
    /// Read or clear the kernel log.
    pub const SYSLOG: usize = 103;
    /// Set a thread's processor mask.
    pub const SCHED_SETAFFINITY: usize = 241;
    /// Read a thread's processor mask, in a buffer a whole number of 32-bit
    /// words long.
    pub const SCHED_GETAFFINITY: usize = 242;
    /// Wait for descriptors with a signal mask and a 64-bit `timespec`, the
    /// sets and the mask pair in 32-bit words. The 32-bit `pselect6` at 308
    /// and `_newselect` at 142 are not carried: their timeouts are `long`s.
    pub const PSELECT6_TIME64: usize = 413;
    /// Report process times into four 32-bit `clock_t`s.
    pub const TIMES: usize = 43;
    /// Report resource use into the 72-byte 32-bit `struct rusage`.
    pub const GETRUSAGE: usize = 77;
    /// Sleep for a relative time in two 32-bit fields. musl calls it for any
    /// request whose seconds fit, before `clock_nanosleep_time64`.
    pub const NANOSLEEP: usize = 162;
    /// Sleep against a chosen clock, with 32-bit `timespec`s.
    pub const CLOCK_NANOSLEEP: usize = 267;
    /// Wait on, or wake, a futex, with a 32-bit `timespec` timeout: what musl's
    /// timed waits call whenever the seconds fit.
    pub const FUTEX: usize = 240;
    /// Read a clock into a 32-bit `timespec`.
    pub const CLOCK_GETTIME: usize = 265;
    /// Read a clock's resolution into a 32-bit `timespec`.
    pub const CLOCK_GETRES: usize = 266;
    /// Set a clock from a 32-bit `timespec`.
    pub const CLOCK_SETTIME: usize = 264;
    /// Poll with a signal mask and a 32-bit `timespec`.
    pub const PPOLL: usize = 309;
    /// Wait for descriptor sets with a signal mask and a 32-bit `timespec`.
    pub const PSELECT6: usize = 308;
    /// Wait for descriptor sets with a 32-bit `timeval`: `select`, whose older
    /// number 82 takes a block of arguments in memory and is not carried.
    pub const NEWSELECT: usize = 142;
    /// Set a file's times from two 32-bit `timespec`s.
    pub const UTIMENSAT: usize = 320;
    /// Arm or disarm a timerfd from 32-bit `timespec`s.
    pub const TIMERFD_SETTIME: usize = 325;
    /// Read a timerfd's time left and interval into 32-bit `timespec`s.
    pub const TIMERFD_GETTIME: usize = 326;
    /// Read the wall clock into a 32-bit `timeval`.
    pub const GETTIMEOFDAY: usize = 78;
    /// Set the wall clock from a 32-bit `timeval`.
    pub const SETTIMEOFDAY: usize = 79;
    /// Read the wall clock in whole seconds, as a 32-bit `time_t`.
    pub const TIME: usize = 13;
    /// Read or adjust the clock discipline through the 128-byte
    /// `old_timex32`.
    pub const ADJTIMEX: usize = 124;
    /// [`ADJTIMEX`] for a named clock.
    pub const CLOCK_ADJTIME: usize = 343;
    /// [`ADJTIMEX`] for a named clock, through the 208-byte `__kernel_timex`.
    pub const CLOCK_ADJTIME64: usize = 405;
    /// Deliver `SIGALRM` after a number of seconds. i386 kept it where ARM's
    /// EABI dropped it, and musl calls it.
    pub const ALARM: usize = 27;
    /// Every socket operation, as a sub-call and a block of 32-bit arguments:
    /// what glibc on i386 calls.
    pub const SOCKETCALL: usize = 102;
    /// Read a resource limit into a 32-bit `struct rlimit`: glibc's
    /// `getrlimit`, whose `RLIM_INFINITY` is `~0` in 32 bits. The older
    /// `getrlimit` at 76, which clamps to `0x7fffffff`, is not carried.
    pub const UGETRLIMIT: usize = 191;
    /// Set a resource limit from a 32-bit `struct rlimit`.
    pub const SETRLIMIT: usize = 75;
    /// Register the robust futex list: a 12-byte 32-bit `robust_list_head`,
    /// which glibc registers in every process and thread.
    pub const SET_ROBUST_LIST: usize = 311;
    /// Read the robust futex list's head and size back, as 32-bit words.
    pub const GET_ROBUST_LIST: usize = 312;
    /// Stat a file by path into i386's 96-byte `struct stat64`: what old
    /// glibc's `__xstat64` calls, which Valve's binaries link against.
    pub const STAT64: usize = 195;
    /// As [`STAT64`], not following a final symbolic link.
    pub const LSTAT64: usize = 196;
    /// As [`STAT64`], for an open file.
    pub const FSTAT64: usize = 197;
    /// As [`STAT64`], relative to a directory descriptor.
    pub const FSTATAT64: usize = 300;
    /// Register restartable sequences; refused, as it is for every program.
    pub const RSEQ: usize = 386;
    /// Wait for a child's change of state, into a 32-bit `siginfo` and
    /// `rusage`.
    pub const WAITID: usize = 284;
    /// Create a socket.
    pub const SOCKET: usize = 359;
    /// Create a connected pair of sockets.
    pub const SOCKETPAIR: usize = 360;
    /// Bind a socket to an address.
    pub const BIND: usize = 361;
    /// Connect a socket to an address.
    pub const CONNECT: usize = 362;
    /// Mark a socket as accepting connections.
    pub const LISTEN: usize = 363;
    /// Accept a connection, with flags for the new descriptor.
    pub const ACCEPT4: usize = 364;
    /// Read a socket option; a `timeval` option is two 32-bit fields.
    pub const GETSOCKOPT: usize = 365;
    /// Set a socket option; a `timeval` option is two 32-bit fields.
    pub const SETSOCKOPT: usize = 366;
    /// Read a socket's own address.
    pub const GETSOCKNAME: usize = 367;
    /// Read the address a socket is connected to.
    pub const GETPEERNAME: usize = 368;
    /// Send a message, to an address if one is given.
    pub const SENDTO: usize = 369;
    /// Send a message described by a 32-bit `msghdr`.
    pub const SENDMSG: usize = 370;
    /// Receive a message and the address it came from.
    pub const RECVFROM: usize = 371;
    /// Receive a message described by a 32-bit `msghdr`.
    pub const RECVMSG: usize = 372;
    /// Shut down one or both directions of a connection.
    pub const SHUTDOWN: usize = 373;
}

/// An architecture-neutral system call.
///
/// The kernel dispatches on this, never on a raw number, so that the two
/// numbering schemes meet in exactly one place: [`from_x86_64`] and
/// [`from_aarch64`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Syscall {
    /// Read bytes from a file descriptor.
    Read,
    /// Write bytes to a file descriptor.
    Write,
    /// Open a file by path. x86-64, ARMv7-A and i386 only.
    Open,
    /// Create a file, or truncate it, and open it for writing. x86-64, ARMv7-A
    /// and i386 only.
    Creat,
    /// Open a file relative to a directory file descriptor.
    Openat,
    /// Open a file from a versioned argument structure.
    Openat2,
    /// Close a file descriptor.
    Close,
    /// Stat a file by path, following symlinks. x86-64 only; ARMv7-A's
    /// equivalent is [`Syscall::Stat64`].
    Stat,
    /// Stat a file by path, following symlinks, into `struct stat64`. ARMv7-A
    /// only: the 32-bit `stat` it replaced cannot express a file larger than 2
    /// GiB.
    Stat64,
    /// Stat an open file descriptor. ARMv7-A's equivalent is
    /// [`Syscall::Fstat64`].
    Fstat,
    /// Stat an open file descriptor into `struct stat64`. ARMv7-A only.
    Fstat64,
    /// Stat a file by path without following a final symlink. x86-64 only;
    /// ARMv7-A's equivalent is [`Syscall::Lstat64`].
    Lstat,
    /// Stat a file by path without following a final symlink, into `struct
    /// stat64`. ARMv7-A only.
    Lstat64,
    /// Stat a file relative to a directory file descriptor.
    Newfstatat,
    /// Stat a file relative to a directory file descriptor, into `struct stat64`.
    /// ARMv7-A's form of [`Syscall::Newfstatat`], which it cannot share because
    /// the structure written back has a different layout.
    Fstatat64,
    /// Stat a file with an explicit field mask and 64-bit timestamps.
    Statx,
    /// Wait for events on a set of file descriptors. x86-64, ARMv7-A and i386 only.
    Poll,
    /// Poll with a signal mask and a `timespec` timeout.
    Ppoll,
    /// Poll with a signal mask and a 64-bit `timespec`. ARMv7-A and i386 only: the timeout
    /// is a pair of 64-bit fields rather than the 32-bit pair [`Syscall::Ppoll`]
    /// takes on those architectures.
    PpollTime64,
    /// Wait for readiness on sets of file descriptors, with a `timeval` timeout.
    /// x86-64 and ARMv7-A only; ARMv7-A calls it `_newselect`.
    Select,
    /// Wait for readiness on descriptor sets, with a signal mask and a `timespec`.
    Pselect6,
    /// Wait for readiness on descriptor sets, with a signal mask and a 64-bit
    /// `timespec`. ARMv7-A and i386 only.
    Pselect6Time64,
    /// Reposition a file descriptor's offset.
    Lseek,
    /// Reposition a file descriptor's offset, with the offset split across two
    /// registers and the result written through a pointer. ARMv7-A and i386 only, because
    /// a 32-bit register cannot carry a 64-bit offset and the return register
    /// cannot carry one back.
    Llseek,
    /// Map files or anonymous memory into the address space.
    Mmap,
    /// Map files or anonymous memory, with the file offset counted in 4096-byte
    /// units. ARMv7-A and i386 only, and *not* interchangeable with [`Syscall::Mmap`]: the
    /// sixth argument must be multiplied by 4096 before use, and a handler that
    /// forgets maps the wrong part of the file.
    Mmap2,
    /// Change the protection of a mapping.
    Mprotect,
    /// Remove a mapping.
    Munmap,
    /// Resize, and possibly move, an existing mapping.
    Mremap,
    /// Flush a file-backed mapping to its file.
    Msync,
    /// Advise the kernel about future use of a memory range.
    Madvise,
    /// Report which pages of a mapping are resident.
    Mincore,
    /// Move the program break, the classic heap boundary.
    Brk,
    /// Install a signal handler.
    RtSigaction,
    /// Change the blocked signal mask.
    RtSigprocmask,
    /// Return from a signal handler, restoring the interrupted context.
    RtSigreturn,
    /// Resume a system call the kernel interrupted to run a handler. The kernel
    /// rewrites the interrupted call's number register to this and rewinds the
    /// program counter, so the call re-enters here rather than as itself; no
    /// program ever issues it. What Linux's `restart_block` drives, for the
    /// calls that resume with a remaining time rather than from the top.
    RestartSyscall,
    /// Return from a handler installed without `SA_SIGINFO`, whose frame has
    /// no `siginfo`. ARMv7-A and i386 only: musl and glibc both point a plain handler's
    /// restorer at it there, where the 64-bit machines have only the `rt` form.
    Sigreturn,
    /// Replace the signal mask and wait for a signal.
    RtSigsuspend,
    /// Sleep until a signal arrives. x86-64, ARMv7-A and i386 only.
    Pause,
    /// Report the blocked signals that are pending.
    RtSigpending,
    /// Wait for one of a set of signals, with a timeout.
    RtSigtimedwait,
    /// Wait for one of a set of signals, with a 64-bit `timespec` timeout.
    /// ARMv7-A only.
    RtSigtimedwaitTime64,
    /// Send a signal with caller-supplied `siginfo` to a process.
    RtSigqueueinfo,
    /// Install or query the alternate signal stack.
    Sigaltstack,
    /// Device-specific control operation on a file descriptor.
    Ioctl,
    /// Read at an explicit offset, leaving the file position alone.
    Pread64,
    /// Write at an explicit offset, leaving the file position alone.
    Pwrite64,
    /// Read into several buffers in one call.
    Readv,
    /// Write from several buffers in one call.
    Writev,
    /// Check a path's accessibility for the real user. x86-64, ARMv7-A and i386 only.
    Access,
    /// Check accessibility relative to a directory file descriptor.
    Faccessat,
    /// Check accessibility with flags, the form musl now prefers.
    Faccessat2,
    /// Create a pipe, returning two file descriptors. x86-64, ARMv7-A and i386 only.
    Pipe,
    /// Create a pipe with flags.
    Pipe2,
    /// Yield the processor to another runnable thread.
    SchedYield,
    /// Duplicate a file descriptor onto the lowest free number.
    Dup,
    /// Duplicate a file descriptor onto a chosen number. x86-64, ARMv7-A and i386 only.
    Dup2,
    /// Duplicate a file descriptor onto a chosen number with flags.
    Dup3,
    /// Sleep for a duration, resumable after a signal.
    Nanosleep,
    /// Read a clock.
    ClockGettime,
    /// Read a clock into a 64-bit `timespec`. ARMv7-A and i386 only. musl has been time64
    /// since 1.2, so this, not [`Syscall::ClockGettime`], is what a current
    /// 32-bit binary calls.
    ClockGettime64,
    /// Read a clock's resolution.
    ClockGetres,
    /// Read a clock's resolution into a 64-bit `timespec`. ARMv7-A and i386 only.
    ClockGetresTime64,
    /// Sleep against a chosen clock, optionally until an absolute time.
    ClockNanosleep,
    /// Sleep against a chosen clock, with 64-bit `timespec` arguments. ARMv7-A and
    /// i386 only.
    ClockNanosleepTime64,
    /// Set the wall clock, the obsolete predecessor of `clock_settime`.
    Settimeofday,
    /// Set a clock.
    ClockSettime,
    /// Set a clock from a 64-bit `timespec`. ARMv7-A and i386 only.
    ClockSettime64,
    /// Read or tune the system clock's discipline.
    Adjtimex,
    /// Read or tune a chosen clock's discipline.
    ClockAdjtime,
    /// Read or tune a chosen clock's discipline, with 64-bit time fields.
    /// ARMv7-A only.
    ClockAdjtime64,
    /// Report the CPU time used by the process and its children.
    Times,
    /// Read an interval timer.
    Getitimer,
    /// Arm or disarm an interval timer.
    Setitimer,
    /// Arrange for `SIGALRM` after a number of seconds. x86-64 only; musl uses
    /// `setitimer` where the table lacks it.
    Alarm,
    /// Read the wall clock, the obsolete predecessor of `clock_gettime`.
    Gettimeofday,
    /// Read the wall clock in whole seconds. x86-64 only: the generic table
    /// never had it and ARM's EABI dropped it, but a static glibc on x86-64
    /// still calls it.
    Time,
    /// Return the calling process's identifier.
    Getpid,
    /// Return the parent process's identifier.
    Getppid,
    /// Return the calling thread's identifier.
    Gettid,
    /// Copy data between two file descriptors inside the kernel.
    Sendfile,
    /// Copy data between two file descriptors, with a 64-bit offset argument.
    /// The 32-bit form of [`Syscall::Sendfile`].
    Sendfile64,
    /// Move bytes between a pipe and another descriptor inside the kernel.
    Splice,
    /// Copy a range of one regular file into another inside the kernel.
    CopyFileRange,
    /// Create a socket.
    Socket,
    /// Connect a socket to an address.
    Connect,
    /// Create a pair of connected sockets.
    Socketpair,
    /// Bind a socket to a local address.
    Bind,
    /// Mark a socket as accepting connections.
    Listen,
    /// Accept a connection on a listening socket.
    Accept,
    /// i386's one entry for every socket call: a sub-call number and a
    /// pointer to its arguments as 32-bit words. i386 only; glibc there
    /// calls it for every socket operation, where musl tries the direct
    /// numbers first.
    Socketcall,
    /// Accept a connection, with flags for the new descriptor.
    Accept4,
    /// Read a socket's local address.
    Getsockname,
    /// Read the address of a socket's peer.
    Getpeername,
    /// Send a message on a socket, optionally to an address.
    Sendto,
    /// Receive a message from a socket, with its source address.
    Recvfrom,
    /// Send a message with ancillary data on a socket.
    Sendmsg,
    /// Receive a message with ancillary data from a socket.
    Recvmsg,
    /// Shut down part or all of a full-duplex connection.
    Shutdown,
    /// Set a socket option.
    Setsockopt,
    /// Read a socket option.
    Getsockopt,
    /// Create or look up a System V shared memory segment.
    Shmget,
    /// Attach a System V shared memory segment.
    Shmat,
    /// Detach a System V shared memory segment.
    Shmdt,
    /// Query or control a System V shared memory segment.
    Shmctl,
    /// Create or look up a System V message queue.
    Msgget,
    /// Send a message to a System V message queue.
    Msgsnd,
    /// Receive a message from a System V message queue.
    Msgrcv,
    /// Query or control a System V message queue.
    Msgctl,
    /// i386's one entry for System V IPC: an operation number, the `IPC_64`
    /// version above it, and that operation's arguments. i386 only.
    Ipc,
    /// Create or look up a System V semaphore set.
    Semget,
    /// Operate on the semaphores in a System V set.
    Semop,
    /// Query or control a System V semaphore set.
    Semctl,
    /// Operate on System V semaphores, giving up after a timeout.
    Semtimedop,
    /// Operate on System V semaphores with a 64-bit `timespec` timeout.
    /// ARMv7-A only.
    SemtimedopTime64,
    /// Create a process or thread; the primitive behind `fork` and `pthread`.
    Clone,
    /// Create a process or thread from a versioned argument structure.
    Clone3,
    /// Create a child process sharing nothing. x86-64, ARMv7-A and i386 only.
    Fork,
    /// Create a child sharing the address space, suspending the parent.
    /// x86-64, ARMv7-A and i386 only.
    Vfork,
    /// Replace the current process image with a program.
    Execve,
    /// Execute a program named by a file descriptor.
    Execveat,
    /// Terminate the calling thread.
    Exit,
    /// Terminate every thread in the process.
    ExitGroup,
    /// Wait for a child to change state, reporting resource usage.
    Wait4,
    /// Wait for a child to change state, without necessarily reaping it.
    Waitid,
    /// Send a signal to a process or process group.
    Kill,
    /// Send a signal to a thread by thread identifier.
    Tkill,
    /// Send a signal to a thread, checked against its thread group.
    Tgkill,
    /// Report kernel name and version.
    Uname,
    /// Set the host name `uname` reports.
    Sethostname,
    /// Set the NIS domain name `uname` reports.
    Setdomainname,
    /// Read or control the kernel log buffer.
    Syslog,
    /// Reboot, halt or power off the machine.
    Reboot,
    /// Read or set the process's execution domain.
    Personality,
    /// Load a kernel module from a buffer.
    InitModule,
    /// Load a kernel module from a file descriptor.
    FinitModule,
    /// Unload a kernel module.
    DeleteModule,
    /// Start swapping to a file or device.
    Swapon,
    /// Stop swapping to a file or device.
    Swapoff,
    /// Move the calling thread into an existing namespace.
    Setns,
    /// Simulate a hangup on the calling process's terminal.
    Vhangup,
    /// Turn process accounting on or off.
    Acct,
    /// Manipulate a file descriptor's flags and locks.
    Fcntl,
    /// Manipulate a file descriptor's flags and locks, with 64-bit `flock64`
    /// structures. ARMv7-A and i386 only; musl calls it for every `fcntl`, not only the
    /// locking commands.
    Fcntl64,
    /// Take or release an advisory lock on a whole open file.
    Flock,
    /// Flush a file's data and metadata to storage.
    Fsync,
    /// Flush a file's data, and only the metadata needed to read it back.
    Fdatasync,
    /// Ask for a range of a file to be read into the page cache ahead of use.
    Readahead,
    /// Flush all file systems.
    Sync,
    /// Set a file's length by path.
    Truncate,
    /// Set a file's length by path, with the length split across two registers.
    /// ARMv7-A and i386 only.
    Truncate64,
    /// Set an open file's length.
    Ftruncate,
    /// Set an open file's length, with the length split across two registers.
    /// ARMv7-A and i386 only.
    Ftruncate64,
    /// Read the current working directory into a buffer.
    Getcwd,
    /// Change the working directory by path.
    Chdir,
    /// Change the working directory to an open directory.
    Fchdir,
    /// Rename a file. x86-64, ARMv7-A and i386 only.
    Rename,
    /// Rename relative to directory file descriptors.
    Renameat,
    /// Rename with flags, such as `RENAME_NOREPLACE`.
    Renameat2,
    /// Create a directory. x86-64, ARMv7-A and i386 only.
    Mkdir,
    /// Create a directory relative to a directory file descriptor.
    Mkdirat,
    /// Create a file, device node, pipe or socket name. x86-64, ARMv7-A and i386
    /// only.
    Mknod,
    /// Create a file, device node, pipe or socket name relative to a
    /// directory file descriptor.
    Mknodat,
    /// Remove an empty directory. x86-64, ARMv7-A and i386 only.
    Rmdir,
    /// Remove a directory entry. x86-64, ARMv7-A and i386 only.
    Unlink,
    /// Remove a directory entry relative to a directory file descriptor.
    Unlinkat,
    /// Create a symbolic link. x86-64, ARMv7-A and i386 only.
    Symlink,
    /// Create a symbolic link relative to a directory file descriptor.
    Symlinkat,
    /// Create a hard link. x86-64, ARMv7-A and i386 only.
    Link,
    /// Create a hard link relative to directory file descriptors.
    Linkat,
    /// Read a symbolic link's target. x86-64, ARMv7-A and i386 only.
    Readlink,
    /// Read a symbolic link relative to a directory file descriptor.
    Readlinkat,
    /// Change a file's mode by path. x86-64, ARMv7-A and i386 only.
    Chmod,
    /// Change an open file's mode.
    Fchmod,
    /// Change a file's mode relative to a directory file descriptor.
    Fchmodat,
    /// Change a file's owner by path. x86-64, ARMv7-A and i386 only.
    Chown,
    /// Change an open file's owner.
    Fchown,
    /// Change a file's owner relative to a directory file descriptor.
    Fchownat,
    /// Change a file's owner by path without following a final symlink.
    /// x86-64, ARMv7-A and i386 only.
    Lchown,
    /// Set a file's access and modification times from two `timespec`s of
    /// this architecture's `long` width.
    Utimensat,
    /// Set a file's access and modification times from two 64-bit
    /// `timespec`s. ARMv7-A and i386 only.
    UtimensatTime64,
    /// Set the file mode creation mask.
    Umask,
    /// Read a resource limit; superseded by `prlimit64`.
    Getrlimit,
    /// Set a resource limit; superseded by `prlimit64`.
    Setrlimit,
    /// Read and set a resource limit of any process in one call.
    Prlimit64,
    /// Encode a file handle for a path.
    NameToHandleAt,
    /// Report accumulated resource usage.
    Getrusage,
    /// Report system-wide memory and load statistics.
    Sysinfo,
    /// Return the real user identifier.
    Getuid,
    /// Return the effective user identifier.
    Geteuid,
    /// Return the real group identifier.
    Getgid,
    /// Return the effective group identifier.
    Getegid,
    /// Set the user identifier.
    Setuid,
    /// Set the group identifier.
    Setgid,
    /// Read the real, effective and saved user identifiers.
    Getresuid,
    /// Read the real, effective and saved group identifiers.
    Getresgid,
    /// Set a process's process-group identifier.
    Setpgid,
    /// Return a process's process-group identifier.
    Getpgid,
    /// Read the supplementary group list.
    Getgroups,
    /// Replace the supplementary group list.
    Setgroups,
    /// Set the real and effective user identifiers.
    Setreuid,
    /// Set the real and effective group identifiers.
    Setregid,
    /// Set the real, effective and saved user identifiers.
    Setresuid,
    /// Set the real, effective and saved group identifiers.
    Setresgid,
    /// Set the user identifier used for file access checks.
    Setfsuid,
    /// Set the group identifier used for file access checks.
    Setfsgid,
    /// Read a thread's capability sets.
    Capget,
    /// Set a thread's capability sets.
    Capset,
    /// Return the calling process's process-group identifier. x86-64, ARMv7-A and i386 only.
    Getpgrp,
    /// Start a new session, with the caller as its leader.
    Setsid,
    /// Return a process's session identifier.
    Getsid,
    /// Read directory entries in the 64-bit layout.
    Getdents64,
    /// Wait on, or wake, a futex; the primitive under every musl lock.
    Futex,
    /// Wait on, or wake, a futex, with a 64-bit `timespec` timeout. ARMv7-A and i386 only,
    /// and the one a time64 musl's locks actually reach.
    FutexTime64,
    /// Register the address cleared and woken on thread exit.
    SetTidAddress,
    /// Register the robust futex list, walked when a thread dies holding a lock.
    SetRobustList,
    /// Read a thread's robust futex list.
    GetRobustList,
    /// Read a thread's processor affinity mask.
    SchedGetaffinity,
    /// Set a thread's processor affinity mask.
    SchedSetaffinity,
    /// Read a thread's scheduling parameters.
    SchedGetparam,
    /// Set a thread's scheduling policy and parameters.
    SchedSetscheduler,
    /// Read a thread's scheduling policy.
    SchedGetscheduler,
    /// Set a thread's scheduling parameters.
    SchedSetparam,
    /// Report the highest priority a scheduling policy allows.
    SchedGetPriorityMax,
    /// Report the lowest priority a scheduling policy allows.
    SchedGetPriorityMin,
    /// Report a thread's round-robin time slice.
    SchedRrGetInterval,
    /// Report a thread's round-robin time slice into a 64-bit `timespec`.
    /// ARMv7-A only.
    SchedRrGetIntervalTime64,
    /// Read the nice value of a process, process group or user.
    Getpriority,
    /// Set the nice value of a process, process group or user.
    Setpriority,
    /// Read a process's I/O scheduling class and priority.
    IoprioGet,
    /// Set a process's I/O scheduling class and priority.
    IoprioSet,
    /// Report the processor and NUMA node the caller is running on.
    Getcpu,
    /// Read or set an architecture register, notably `FS_BASE`. x86-64 only;
    /// ARMv7-A reaches the same effect through [`Syscall::ArmSetTls`].
    ArchPrctl,
    /// Set the thread pointer read through `TPIDRURO`. ARMv7-A only, and the
    /// reason ARM needs no [`Syscall::ArchPrctl`]: a 32-bit ARM userspace cannot
    /// write the register itself, so setting up thread-local storage is a call
    /// into the kernel.
    ArmSetTls,
    /// Install one of the calling thread's three thread-local segment
    /// descriptors from a `struct user_desc`. i386 only: its thread pointer
    /// is a segment (`docs/I386.md` §3.5).
    SetThreadArea,
    /// Read one of the calling thread's thread-local segment descriptors back
    /// as a `struct user_desc`. i386 only.
    GetThreadArea,
    /// Make a range of memory coherent between the data and instruction caches.
    /// ARMv7-A only. Unlike x86-64 and AArch64, a 32-bit ARM userspace cannot
    /// reach the cache maintenance operations, so anything that writes code — a
    /// JIT, a trampoline — has to ask the kernel.
    ArmCacheflush,
    /// Operate on per-process control settings, such as the thread name.
    Prctl,
    /// Restrict the system calls a thread may make, or ask what seccomp offers.
    Seccomp,
    /// Fill a buffer with random bytes.
    Getrandom,
    /// Create an anonymous file living in memory.
    MemfdCreate,
    /// Create an epoll set with flags.
    EpollCreate1,
    /// Add, modify or remove a file descriptor in an epoll set.
    EpollCtl,
    /// Wait for events on an epoll set. x86-64, ARMv7-A and i386 only; AArch64 has
    /// only [`Syscall::EpollPwait`].
    EpollWait,
    /// Wait on an epoll set with a signal mask.
    EpollPwait,
    /// Create an epoll set, the size a hint. x86-64, ARMv7-A and i386 only.
    EpollCreate,
    /// Wait on an epoll set with a signal mask and a `struct timespec`
    /// timeout of two 64-bit words on every architecture.
    EpollPwait2,
    /// Create an eventfd with flags.
    Eventfd2,
    /// Create an eventfd with no flags. x86-64, ARMv7-A and i386 only.
    Eventfd,
    /// Create a signalfd, or change one's mask, with flags.
    Signalfd4,
    /// Create a signalfd, or change one's mask, with no flags. x86-64, ARMv7-A
    /// and i386 only.
    Signalfd,
    /// Make an inotify instance, with no flags. x86-64, ARMv7-A and i386 only.
    InotifyInit,
    /// Make an inotify instance, with flags.
    InotifyInit1,
    /// Watch a path on an inotify instance.
    InotifyAddWatch,
    /// Remove a watch from an inotify instance.
    InotifyRmWatch,
    /// Make a pidfd for a process.
    PidfdOpen,
    /// Send a signal through a pidfd.
    PidfdSendSignal,
    /// Create a timer that expires into a descriptor.
    TimerfdCreate,
    /// Arm or disarm a timerfd, from native-width `itimerspec`s.
    TimerfdSettime,
    /// Read a timerfd's time left and interval, as a native-width
    /// `itimerspec`.
    TimerfdGettime,
    /// [`Syscall::TimerfdSettime`] with 64-bit `timespec` fields. ARMv7-A and
    /// i386 only: a time64 musl calls this, not the 32-bit form.
    TimerfdSettime64,
    /// [`Syscall::TimerfdGettime`] with 64-bit `timespec` fields. ARMv7-A and
    /// i386 only.
    TimerfdGettime64,
    /// Issue a process-wide memory barrier.
    Membarrier,
    /// Register a restartable sequence area.
    Rseq,
    /// Detach parts of the calling process's shared execution context.
    Unshare,
    /// Attach a file system.
    Mount,
    /// Detach a file system.
    Umount2,
    /// Report file system statistics by path.
    Statfs,
    /// Report file system statistics by path, into `struct statfs64`. ARMv7-A and
    /// i386 only, and it takes the structure's size as an explicit second argument,
    /// which [`Syscall::Statfs`] does not.
    Statfs64,
    /// Report file system statistics for an open file.
    Fstatfs,
    /// Report file system statistics for an open file, into `struct statfs64`.
    /// ARMv7-A and i386 only, with the same explicit size argument as
    /// [`Syscall::Statfs64`].
    Fstatfs64,
    /// Set an extended attribute by path, following symbolic links.
    Setxattr,
    /// Set an extended attribute by path, on a symbolic link itself.
    Lsetxattr,
    /// Set an extended attribute of an open file.
    Fsetxattr,
    /// Read an extended attribute by path, following symbolic links.
    Getxattr,
    /// Read an extended attribute by path, of a symbolic link itself.
    Lgetxattr,
    /// Read an extended attribute of an open file.
    Fgetxattr,
    /// List extended attribute names by path, following symbolic links.
    Listxattr,
    /// List extended attribute names by path, of a symbolic link itself.
    Llistxattr,
    /// List extended attribute names of an open file.
    Flistxattr,
    /// Remove an extended attribute by path, following symbolic links.
    Removexattr,
    /// Remove an extended attribute by path, from a symbolic link itself.
    Lremovexattr,
    /// Remove an extended attribute of an open file.
    Fremovexattr,
    /// Make another mount the root, and move the old root beneath it.
    PivotRoot,
    /// Reserve or release storage for a range of an open file.
    Fallocate,
    /// Change the calling process's root directory.
    Chroot,
    /// Flush the file system holding an open file.
    Syncfs,
}

/// One past the highest number [`from_x86_64`] translates.
///
/// A number at or above it is `ENOSYS` without asking the table. The kernel
/// bounds a program's number by it *before* the table is asked, and clamps it
/// so that a mispredicted bound cannot steer the table's own lookup -- a jump
/// table indexed by the number, once the compiler is done with the match --
/// past its end (Spectre variant 1). A host test holds it to the table: no
/// number at or above it translates, and the one below it does.
pub const X86_64_END: usize = 442;

/// One past the highest number [`from_aarch64`] translates. As
/// [`X86_64_END`].
pub const AARCH64_END: usize = 442;

/// One past the highest number [`from_arm`] translates in the shared EABI
/// table. The ARM-private calls above [`arm::ARM_PRIVATE_BASE`] are bounded
/// by [`ARM_PRIVATE_END`] instead. As [`X86_64_END`].
pub const ARM_END: usize = 442;

/// One past the highest ARM-private number [`from_arm`] translates.
pub const ARM_PRIVATE_END: usize = arm::ARM_SET_TLS + 1;

/// One past the highest number [`from_i386`] translates. As
/// [`X86_64_END`], and it moves up as calls are mapped.
pub const I386_END: usize = i386::EPOLL_PWAIT2 + 1;

/// Translate an x86-64 system call number.
///
/// Returns [`None`] for a number this crate does not know, which the caller
/// reports as `ENOSYS`. Split into helpers -- the older calls by number range,
/// the rest by subject -- because one match over the whole table would be both
/// unreadably long and a `too_many_lines` failure.
#[must_use]
pub fn from_x86_64(nr: usize) -> Option<Syscall> {
    // Asked in turn, for the reason given on `from_aarch64`.
    x86_64_file_and_process(nr)
        .or_else(|| x86_64_metadata_and_ids(nr))
        .or_else(|| x86_64_threads_and_time(nr))
        .or_else(|| x86_64_at_family(nr))
        .or_else(|| x86_64_recent(nr))
        .or_else(|| x86_64_sockets_and_shm(nr))
        .or_else(|| x86_64_credentials_and_sessions(nr))
        .or_else(|| x86_64_administration(nr))
        .or_else(|| x86_64_clocks_and_timers(nr))
        .or_else(|| x86_64_signals_and_scheduling(nr))
}

/// x86-64 numbers 0 to 63: the original UNIX core.
fn x86_64_file_and_process(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::READ => Syscall::Read,
        x86_64::WRITE => Syscall::Write,
        x86_64::OPEN => Syscall::Open,
        x86_64::CREAT => Syscall::Creat,
        x86_64::CLOSE => Syscall::Close,
        x86_64::STAT => Syscall::Stat,
        x86_64::FSTAT => Syscall::Fstat,
        x86_64::LSTAT => Syscall::Lstat,
        x86_64::POLL => Syscall::Poll,
        x86_64::LSEEK => Syscall::Lseek,
        x86_64::MMAP => Syscall::Mmap,
        x86_64::MPROTECT => Syscall::Mprotect,
        x86_64::MUNMAP => Syscall::Munmap,
        x86_64::BRK => Syscall::Brk,
        x86_64::RT_SIGACTION => Syscall::RtSigaction,
        x86_64::RT_SIGPROCMASK => Syscall::RtSigprocmask,
        x86_64::RT_SIGRETURN => Syscall::RtSigreturn,
        x86_64::RESTART_SYSCALL => Syscall::RestartSyscall,
        x86_64::IOCTL => Syscall::Ioctl,
        x86_64::PREAD64 => Syscall::Pread64,
        x86_64::PWRITE64 => Syscall::Pwrite64,
        x86_64::READV => Syscall::Readv,
        x86_64::WRITEV => Syscall::Writev,
        x86_64::ACCESS => Syscall::Access,
        x86_64::PIPE => Syscall::Pipe,
        x86_64::SCHED_YIELD => Syscall::SchedYield,
        x86_64::MREMAP => Syscall::Mremap,
        x86_64::MSYNC => Syscall::Msync,
        x86_64::MADVISE => Syscall::Madvise,
        x86_64::MINCORE => Syscall::Mincore,
        x86_64::DUP => Syscall::Dup,
        x86_64::DUP2 => Syscall::Dup2,
        x86_64::NANOSLEEP => Syscall::Nanosleep,
        x86_64::GETPID => Syscall::Getpid,
        x86_64::SENDFILE => Syscall::Sendfile,
        x86_64::SPLICE => Syscall::Splice,
        x86_64::COPY_FILE_RANGE => Syscall::CopyFileRange,
        x86_64::CLONE => Syscall::Clone,
        x86_64::FORK => Syscall::Fork,
        x86_64::VFORK => Syscall::Vfork,
        x86_64::EXECVE => Syscall::Execve,
        x86_64::EXIT => Syscall::Exit,
        x86_64::WAIT4 => Syscall::Wait4,
        x86_64::KILL => Syscall::Kill,
        x86_64::UNAME => Syscall::Uname,
        _ => return None,
    };
    Some(call)
}

/// x86-64 numbers 64 to 159: file metadata, credentials and signals.
fn x86_64_metadata_and_ids(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::FCNTL => Syscall::Fcntl,
        x86_64::FSYNC => Syscall::Fsync,
        x86_64::FDATASYNC => Syscall::Fdatasync,
        x86_64::TRUNCATE => Syscall::Truncate,
        x86_64::FTRUNCATE => Syscall::Ftruncate,
        x86_64::GETCWD => Syscall::Getcwd,
        x86_64::CHDIR => Syscall::Chdir,
        x86_64::FCHDIR => Syscall::Fchdir,
        x86_64::RENAME => Syscall::Rename,
        x86_64::MKDIR => Syscall::Mkdir,
        x86_64::RMDIR => Syscall::Rmdir,
        x86_64::LINK => Syscall::Link,
        x86_64::UNLINK => Syscall::Unlink,
        x86_64::SYMLINK => Syscall::Symlink,
        x86_64::READLINK => Syscall::Readlink,
        x86_64::CHMOD => Syscall::Chmod,
        x86_64::FCHMOD => Syscall::Fchmod,
        x86_64::CHOWN => Syscall::Chown,
        x86_64::FCHOWN => Syscall::Fchown,
        x86_64::LCHOWN => Syscall::Lchown,
        x86_64::UMASK => Syscall::Umask,
        x86_64::GETTIMEOFDAY => Syscall::Gettimeofday,
        x86_64::GETRLIMIT => Syscall::Getrlimit,
        x86_64::GETRUSAGE => Syscall::Getrusage,
        x86_64::SYSINFO => Syscall::Sysinfo,
        x86_64::GETUID => Syscall::Getuid,
        x86_64::GETGID => Syscall::Getgid,
        x86_64::SETUID => Syscall::Setuid,
        x86_64::SETGID => Syscall::Setgid,
        x86_64::GETEUID => Syscall::Geteuid,
        x86_64::GETEGID => Syscall::Getegid,
        x86_64::SETPGID => Syscall::Setpgid,
        x86_64::GETPPID => Syscall::Getppid,
        x86_64::GETGROUPS => Syscall::Getgroups,
        x86_64::GETRESUID => Syscall::Getresuid,
        x86_64::GETRESGID => Syscall::Getresgid,
        x86_64::GETPGID => Syscall::Getpgid,
        x86_64::RT_SIGSUSPEND => Syscall::RtSigsuspend,
        x86_64::SIGALTSTACK => Syscall::Sigaltstack,
        x86_64::MKNOD => Syscall::Mknod,
        x86_64::STATFS => Syscall::Statfs,
        x86_64::FSTATFS => Syscall::Fstatfs,
        x86_64::SCHED_GETPARAM => Syscall::SchedGetparam,
        x86_64::SCHED_SETSCHEDULER => Syscall::SchedSetscheduler,
        x86_64::SCHED_GETSCHEDULER => Syscall::SchedGetscheduler,
        x86_64::PRCTL => Syscall::Prctl,
        x86_64::ARCH_PRCTL => Syscall::ArchPrctl,
        x86_64::FLOCK => Syscall::Flock,
        _ => return None,
    };
    Some(call)
}

/// x86-64 numbers 160 to 255: threads, futexes, clocks and epoll.
fn x86_64_threads_and_time(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::SETRLIMIT => Syscall::Setrlimit,
        x86_64::SYNC => Syscall::Sync,
        x86_64::MOUNT => Syscall::Mount,
        x86_64::UMOUNT2 => Syscall::Umount2,
        x86_64::GETTID => Syscall::Gettid,
        x86_64::TKILL => Syscall::Tkill,
        x86_64::TIME => Syscall::Time,
        x86_64::FUTEX => Syscall::Futex,
        x86_64::SCHED_SETAFFINITY => Syscall::SchedSetaffinity,
        x86_64::SCHED_GETAFFINITY => Syscall::SchedGetaffinity,
        x86_64::GETDENTS64 => Syscall::Getdents64,
        x86_64::SET_TID_ADDRESS => Syscall::SetTidAddress,
        x86_64::CLOCK_GETTIME => Syscall::ClockGettime,
        x86_64::CLOCK_GETRES => Syscall::ClockGetres,
        x86_64::CLOCK_NANOSLEEP => Syscall::ClockNanosleep,
        x86_64::EXIT_GROUP => Syscall::ExitGroup,
        x86_64::EPOLL_CREATE => Syscall::EpollCreate,
        x86_64::EPOLL_WAIT => Syscall::EpollWait,
        x86_64::EPOLL_CTL => Syscall::EpollCtl,
        x86_64::TGKILL => Syscall::Tgkill,
        x86_64::WAITID => Syscall::Waitid,
        x86_64::READAHEAD => Syscall::Readahead,
        _ => return None,
    };
    Some(call)
}

/// x86-64 numbers 256 to 349: the `*at` family and its contemporaries.
fn x86_64_at_family(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::OPENAT => Syscall::Openat,
        x86_64::MKDIRAT => Syscall::Mkdirat,
        x86_64::MKNODAT => Syscall::Mknodat,
        x86_64::FCHOWNAT => Syscall::Fchownat,
        x86_64::NEWFSTATAT => Syscall::Newfstatat,
        x86_64::UNLINKAT => Syscall::Unlinkat,
        x86_64::RENAMEAT => Syscall::Renameat,
        x86_64::LINKAT => Syscall::Linkat,
        x86_64::SYMLINKAT => Syscall::Symlinkat,
        x86_64::READLINKAT => Syscall::Readlinkat,
        x86_64::FCHMODAT => Syscall::Fchmodat,
        x86_64::FACCESSAT => Syscall::Faccessat,
        x86_64::PPOLL => Syscall::Ppoll,
        x86_64::UNSHARE => Syscall::Unshare,
        x86_64::SET_ROBUST_LIST => Syscall::SetRobustList,
        x86_64::GET_ROBUST_LIST => Syscall::GetRobustList,
        x86_64::UTIMENSAT => Syscall::Utimensat,
        x86_64::EPOLL_PWAIT => Syscall::EpollPwait,
        x86_64::EVENTFD => Syscall::Eventfd,
        x86_64::EVENTFD2 => Syscall::Eventfd2,
        x86_64::SIGNALFD => Syscall::Signalfd,
        x86_64::SIGNALFD4 => Syscall::Signalfd4,
        x86_64::EPOLL_CREATE1 => Syscall::EpollCreate1,
        x86_64::DUP3 => Syscall::Dup3,
        x86_64::PIPE2 => Syscall::Pipe2,
        x86_64::PRLIMIT64 => Syscall::Prlimit64,
        x86_64::NAME_TO_HANDLE_AT => Syscall::NameToHandleAt,
        x86_64::RENAMEAT2 => Syscall::Renameat2,
        x86_64::SECCOMP => Syscall::Seccomp,
        x86_64::GETRANDOM => Syscall::Getrandom,
        x86_64::MEMFD_CREATE => Syscall::MemfdCreate,
        x86_64::EXECVEAT => Syscall::Execveat,
        x86_64::MEMBARRIER => Syscall::Membarrier,
        x86_64::STATX => Syscall::Statx,
        x86_64::RSEQ => Syscall::Rseq,
        _ => return None,
    };
    Some(call)
}

/// x86-64 numbers from 350 up: calls added after the number spaces converged.
fn x86_64_recent(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::INOTIFY_INIT => Syscall::InotifyInit,
        x86_64::INOTIFY_INIT1 => Syscall::InotifyInit1,
        x86_64::INOTIFY_ADD_WATCH => Syscall::InotifyAddWatch,
        x86_64::INOTIFY_RM_WATCH => Syscall::InotifyRmWatch,
        x86_64::PIDFD_OPEN => Syscall::PidfdOpen,
        x86_64::PIDFD_SEND_SIGNAL => Syscall::PidfdSendSignal,
        x86_64::CLONE3 => Syscall::Clone3,
        x86_64::OPENAT2 => Syscall::Openat2,
        x86_64::FACCESSAT2 => Syscall::Faccessat2,
        x86_64::EPOLL_PWAIT2 => Syscall::EpollPwait2,
        x86_64::SETXATTR => Syscall::Setxattr,
        x86_64::LSETXATTR => Syscall::Lsetxattr,
        x86_64::FSETXATTR => Syscall::Fsetxattr,
        x86_64::GETXATTR => Syscall::Getxattr,
        x86_64::LGETXATTR => Syscall::Lgetxattr,
        x86_64::FGETXATTR => Syscall::Fgetxattr,
        x86_64::LISTXATTR => Syscall::Listxattr,
        x86_64::LLISTXATTR => Syscall::Llistxattr,
        x86_64::FLISTXATTR => Syscall::Flistxattr,
        x86_64::REMOVEXATTR => Syscall::Removexattr,
        x86_64::LREMOVEXATTR => Syscall::Lremovexattr,
        x86_64::FREMOVEXATTR => Syscall::Fremovexattr,
        x86_64::PIVOT_ROOT => Syscall::PivotRoot,
        x86_64::FALLOCATE => Syscall::Fallocate,
        x86_64::CHROOT => Syscall::Chroot,
        x86_64::SYNCFS => Syscall::Syncfs,
        _ => return None,
    };
    Some(call)
}

/// x86-64 sockets and System V IPC, wherever they sit in the table.
fn x86_64_sockets_and_shm(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::SHMGET => Syscall::Shmget,
        x86_64::SHMAT => Syscall::Shmat,
        x86_64::SHMCTL => Syscall::Shmctl,
        x86_64::SOCKET => Syscall::Socket,
        x86_64::CONNECT => Syscall::Connect,
        x86_64::ACCEPT => Syscall::Accept,
        x86_64::SENDTO => Syscall::Sendto,
        x86_64::RECVFROM => Syscall::Recvfrom,
        x86_64::SENDMSG => Syscall::Sendmsg,
        x86_64::RECVMSG => Syscall::Recvmsg,
        x86_64::SHUTDOWN => Syscall::Shutdown,
        x86_64::BIND => Syscall::Bind,
        x86_64::LISTEN => Syscall::Listen,
        x86_64::GETSOCKNAME => Syscall::Getsockname,
        x86_64::GETPEERNAME => Syscall::Getpeername,
        x86_64::SOCKETPAIR => Syscall::Socketpair,
        x86_64::SETSOCKOPT => Syscall::Setsockopt,
        x86_64::GETSOCKOPT => Syscall::Getsockopt,
        x86_64::SHMDT => Syscall::Shmdt,
        x86_64::ACCEPT4 => Syscall::Accept4,
        x86_64::MSGGET => Syscall::Msgget,
        x86_64::MSGSND => Syscall::Msgsnd,
        x86_64::MSGRCV => Syscall::Msgrcv,
        x86_64::MSGCTL => Syscall::Msgctl,
        x86_64::SEMGET => Syscall::Semget,
        x86_64::SEMOP => Syscall::Semop,
        x86_64::SEMCTL => Syscall::Semctl,
        x86_64::SEMTIMEDOP => Syscall::Semtimedop,
        _ => return None,
    };
    Some(call)
}

/// x86-64 credential changes, capabilities, process groups and sessions.
fn x86_64_credentials_and_sessions(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::GETPGRP => Syscall::Getpgrp,
        x86_64::SETSID => Syscall::Setsid,
        x86_64::SETREUID => Syscall::Setreuid,
        x86_64::SETREGID => Syscall::Setregid,
        x86_64::SETGROUPS => Syscall::Setgroups,
        x86_64::SETRESUID => Syscall::Setresuid,
        x86_64::SETRESGID => Syscall::Setresgid,
        x86_64::SETFSUID => Syscall::Setfsuid,
        x86_64::SETFSGID => Syscall::Setfsgid,
        x86_64::GETSID => Syscall::Getsid,
        x86_64::CAPGET => Syscall::Capget,
        x86_64::CAPSET => Syscall::Capset,
        _ => return None,
    };
    Some(call)
}

/// x86-64 host names, kernel modules, swap, namespaces and accounting.
fn x86_64_administration(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::SYSLOG => Syscall::Syslog,
        x86_64::PERSONALITY => Syscall::Personality,
        x86_64::VHANGUP => Syscall::Vhangup,
        x86_64::ACCT => Syscall::Acct,
        x86_64::SWAPON => Syscall::Swapon,
        x86_64::SWAPOFF => Syscall::Swapoff,
        x86_64::REBOOT => Syscall::Reboot,
        x86_64::SETHOSTNAME => Syscall::Sethostname,
        x86_64::SETDOMAINNAME => Syscall::Setdomainname,
        x86_64::INIT_MODULE => Syscall::InitModule,
        x86_64::DELETE_MODULE => Syscall::DeleteModule,
        x86_64::SETNS => Syscall::Setns,
        x86_64::FINIT_MODULE => Syscall::FinitModule,
        _ => return None,
    };
    Some(call)
}

/// x86-64 clock setting and tuning, and interval timers.
fn x86_64_clocks_and_timers(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::GETITIMER => Syscall::Getitimer,
        x86_64::ALARM => Syscall::Alarm,
        x86_64::SETITIMER => Syscall::Setitimer,
        x86_64::TIMES => Syscall::Times,
        x86_64::ADJTIMEX => Syscall::Adjtimex,
        x86_64::SETTIMEOFDAY => Syscall::Settimeofday,
        x86_64::CLOCK_SETTIME => Syscall::ClockSettime,
        x86_64::CLOCK_ADJTIME => Syscall::ClockAdjtime,
        x86_64::TIMERFD_CREATE => Syscall::TimerfdCreate,
        x86_64::TIMERFD_SETTIME => Syscall::TimerfdSettime,
        x86_64::TIMERFD_GETTIME => Syscall::TimerfdGettime,
        _ => return None,
    };
    Some(call)
}

/// x86-64 descriptor and signal waits, and scheduling priorities.
fn x86_64_signals_and_scheduling(nr: usize) -> Option<Syscall> {
    let call = match nr {
        x86_64::SELECT => Syscall::Select,
        x86_64::PAUSE => Syscall::Pause,
        x86_64::RT_SIGPENDING => Syscall::RtSigpending,
        x86_64::RT_SIGTIMEDWAIT => Syscall::RtSigtimedwait,
        x86_64::RT_SIGQUEUEINFO => Syscall::RtSigqueueinfo,
        x86_64::GETPRIORITY => Syscall::Getpriority,
        x86_64::SETPRIORITY => Syscall::Setpriority,
        x86_64::SCHED_SETPARAM => Syscall::SchedSetparam,
        x86_64::SCHED_GET_PRIORITY_MAX => Syscall::SchedGetPriorityMax,
        x86_64::SCHED_GET_PRIORITY_MIN => Syscall::SchedGetPriorityMin,
        x86_64::SCHED_RR_GET_INTERVAL => Syscall::SchedRrGetInterval,
        x86_64::IOPRIO_SET => Syscall::IoprioSet,
        x86_64::IOPRIO_GET => Syscall::IoprioGet,
        x86_64::PSELECT6 => Syscall::Pselect6,
        x86_64::GETCPU => Syscall::Getcpu,
        _ => return None,
    };
    Some(call)
}

/// Translate an AArch64 system call number.
///
/// Returns [`None`] both for a number this crate does not know and for one the
/// generic table never assigned, which is the same answer: `ENOSYS`.
#[must_use]
pub fn from_aarch64(nr: usize) -> Option<Syscall> {
    // Each helper is asked in turn rather than selected by range. The grouping
    // exists to keep any one function under the line limit, and it should not
    // be able to affect the answer -- when it could, `socket` (198) sat in the
    // 200..=299 helper and was unreachable, which is the kind of hole a
    // dispatch table must not have.
    aarch64_files(nr)
        .or_else(|| aarch64_signals_and_ids(nr))
        .or_else(|| aarch64_memory_and_process(nr))
        .or_else(|| aarch64_recent(nr))
        .or_else(|| aarch64_sockets_and_shm(nr))
        .or_else(|| aarch64_credentials_and_sessions(nr))
        .or_else(|| aarch64_administration(nr))
        .or_else(|| aarch64_clocks_and_timers(nr))
        .or_else(|| aarch64_signals_and_scheduling(nr))
}

/// AArch64 numbers 0 to 99: descriptors, paths and process exit.
fn aarch64_files(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::GETCWD => Syscall::Getcwd,
        aarch64::EVENTFD2 => Syscall::Eventfd2,
        aarch64::EPOLL_CREATE1 => Syscall::EpollCreate1,
        aarch64::EPOLL_CTL => Syscall::EpollCtl,
        aarch64::EPOLL_PWAIT => Syscall::EpollPwait,
        aarch64::DUP => Syscall::Dup,
        aarch64::DUP3 => Syscall::Dup3,
        aarch64::FCNTL => Syscall::Fcntl,
        aarch64::IOCTL => Syscall::Ioctl,
        aarch64::MKNODAT => Syscall::Mknodat,
        aarch64::MKDIRAT => Syscall::Mkdirat,
        aarch64::UNLINKAT => Syscall::Unlinkat,
        aarch64::SYMLINKAT => Syscall::Symlinkat,
        aarch64::LINKAT => Syscall::Linkat,
        aarch64::RENAMEAT => Syscall::Renameat,
        aarch64::RENAMEAT2 => Syscall::Renameat2,
        aarch64::UMOUNT2 => Syscall::Umount2,
        aarch64::MOUNT => Syscall::Mount,
        aarch64::STATFS => Syscall::Statfs,
        aarch64::FSTATFS => Syscall::Fstatfs,
        aarch64::TRUNCATE => Syscall::Truncate,
        aarch64::FTRUNCATE => Syscall::Ftruncate,
        aarch64::FACCESSAT => Syscall::Faccessat,
        aarch64::CHDIR => Syscall::Chdir,
        aarch64::FCHDIR => Syscall::Fchdir,
        aarch64::FCHMOD => Syscall::Fchmod,
        aarch64::FCHMODAT => Syscall::Fchmodat,
        aarch64::FCHOWNAT => Syscall::Fchownat,
        aarch64::FCHOWN => Syscall::Fchown,
        aarch64::OPENAT => Syscall::Openat,
        aarch64::CLOSE => Syscall::Close,
        aarch64::PIPE2 => Syscall::Pipe2,
        aarch64::GETDENTS64 => Syscall::Getdents64,
        aarch64::LSEEK => Syscall::Lseek,
        aarch64::READ => Syscall::Read,
        aarch64::WRITE => Syscall::Write,
        aarch64::READV => Syscall::Readv,
        aarch64::WRITEV => Syscall::Writev,
        aarch64::PREAD64 => Syscall::Pread64,
        aarch64::PWRITE64 => Syscall::Pwrite64,
        aarch64::SENDFILE => Syscall::Sendfile,
        aarch64::SPLICE => Syscall::Splice,
        aarch64::COPY_FILE_RANGE => Syscall::CopyFileRange,
        aarch64::PPOLL => Syscall::Ppoll,
        aarch64::SIGNALFD4 => Syscall::Signalfd4,
        aarch64::READLINKAT => Syscall::Readlinkat,
        aarch64::NEWFSTATAT => Syscall::Newfstatat,
        aarch64::FSTAT => Syscall::Fstat,
        aarch64::SYNC => Syscall::Sync,
        aarch64::FSYNC => Syscall::Fsync,
        aarch64::FDATASYNC => Syscall::Fdatasync,
        aarch64::UTIMENSAT => Syscall::Utimensat,
        aarch64::EXIT => Syscall::Exit,
        aarch64::EXIT_GROUP => Syscall::ExitGroup,
        aarch64::WAITID => Syscall::Waitid,
        aarch64::SET_TID_ADDRESS => Syscall::SetTidAddress,
        aarch64::UNSHARE => Syscall::Unshare,
        aarch64::FUTEX => Syscall::Futex,
        aarch64::SET_ROBUST_LIST => Syscall::SetRobustList,
        aarch64::FLOCK => Syscall::Flock,
        _ => return None,
    };
    Some(call)
}

/// AArch64 numbers 100 to 199: clocks, scheduling, signals and credentials.
fn aarch64_signals_and_ids(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::GET_ROBUST_LIST => Syscall::GetRobustList,
        aarch64::NANOSLEEP => Syscall::Nanosleep,
        aarch64::CLOCK_GETTIME => Syscall::ClockGettime,
        aarch64::CLOCK_GETRES => Syscall::ClockGetres,
        aarch64::CLOCK_NANOSLEEP => Syscall::ClockNanosleep,
        aarch64::SCHED_SETSCHEDULER => Syscall::SchedSetscheduler,
        aarch64::SCHED_GETSCHEDULER => Syscall::SchedGetscheduler,
        aarch64::SCHED_GETPARAM => Syscall::SchedGetparam,
        aarch64::SCHED_SETAFFINITY => Syscall::SchedSetaffinity,
        aarch64::SCHED_GETAFFINITY => Syscall::SchedGetaffinity,
        aarch64::SCHED_YIELD => Syscall::SchedYield,
        aarch64::KILL => Syscall::Kill,
        aarch64::TKILL => Syscall::Tkill,
        aarch64::TGKILL => Syscall::Tgkill,
        aarch64::SIGALTSTACK => Syscall::Sigaltstack,
        aarch64::RT_SIGSUSPEND => Syscall::RtSigsuspend,
        aarch64::RT_SIGACTION => Syscall::RtSigaction,
        aarch64::RT_SIGPROCMASK => Syscall::RtSigprocmask,
        aarch64::RT_SIGRETURN => Syscall::RtSigreturn,
        aarch64::RESTART_SYSCALL => Syscall::RestartSyscall,
        aarch64::SETGID => Syscall::Setgid,
        aarch64::SETUID => Syscall::Setuid,
        aarch64::GETRESUID => Syscall::Getresuid,
        aarch64::GETRESGID => Syscall::Getresgid,
        aarch64::SETPGID => Syscall::Setpgid,
        aarch64::GETPGID => Syscall::Getpgid,
        aarch64::GETGROUPS => Syscall::Getgroups,
        aarch64::UNAME => Syscall::Uname,
        aarch64::GETRLIMIT => Syscall::Getrlimit,
        aarch64::SETRLIMIT => Syscall::Setrlimit,
        aarch64::GETRUSAGE => Syscall::Getrusage,
        aarch64::UMASK => Syscall::Umask,
        aarch64::PRCTL => Syscall::Prctl,
        aarch64::GETTIMEOFDAY => Syscall::Gettimeofday,
        aarch64::GETPID => Syscall::Getpid,
        aarch64::GETPPID => Syscall::Getppid,
        aarch64::GETUID => Syscall::Getuid,
        aarch64::GETEUID => Syscall::Geteuid,
        aarch64::GETGID => Syscall::Getgid,
        aarch64::GETEGID => Syscall::Getegid,
        aarch64::GETTID => Syscall::Gettid,
        aarch64::SYSINFO => Syscall::Sysinfo,
        _ => return None,
    };
    Some(call)
}

/// AArch64 numbers 200 to 299: sockets, memory and process creation.
fn aarch64_memory_and_process(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::BRK => Syscall::Brk,
        aarch64::MUNMAP => Syscall::Munmap,
        aarch64::MREMAP => Syscall::Mremap,
        aarch64::CLONE => Syscall::Clone,
        aarch64::EXECVE => Syscall::Execve,
        aarch64::MMAP => Syscall::Mmap,
        aarch64::MPROTECT => Syscall::Mprotect,
        aarch64::MSYNC => Syscall::Msync,
        aarch64::MADVISE => Syscall::Madvise,
        aarch64::MINCORE => Syscall::Mincore,
        aarch64::WAIT4 => Syscall::Wait4,
        aarch64::PRLIMIT64 => Syscall::Prlimit64,
        aarch64::NAME_TO_HANDLE_AT => Syscall::NameToHandleAt,
        aarch64::SECCOMP => Syscall::Seccomp,
        aarch64::GETRANDOM => Syscall::Getrandom,
        aarch64::MEMFD_CREATE => Syscall::MemfdCreate,
        aarch64::EXECVEAT => Syscall::Execveat,
        aarch64::MEMBARRIER => Syscall::Membarrier,
        aarch64::STATX => Syscall::Statx,
        aarch64::RSEQ => Syscall::Rseq,
        aarch64::READAHEAD => Syscall::Readahead,
        _ => return None,
    };
    Some(call)
}

/// AArch64 numbers from 300 up: calls added after the number spaces converged.
fn aarch64_recent(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::INOTIFY_INIT1 => Syscall::InotifyInit1,
        aarch64::INOTIFY_ADD_WATCH => Syscall::InotifyAddWatch,
        aarch64::INOTIFY_RM_WATCH => Syscall::InotifyRmWatch,
        aarch64::PIDFD_OPEN => Syscall::PidfdOpen,
        aarch64::PIDFD_SEND_SIGNAL => Syscall::PidfdSendSignal,
        aarch64::CLONE3 => Syscall::Clone3,
        aarch64::OPENAT2 => Syscall::Openat2,
        aarch64::FACCESSAT2 => Syscall::Faccessat2,
        aarch64::EPOLL_PWAIT2 => Syscall::EpollPwait2,
        aarch64::SETXATTR => Syscall::Setxattr,
        aarch64::LSETXATTR => Syscall::Lsetxattr,
        aarch64::FSETXATTR => Syscall::Fsetxattr,
        aarch64::GETXATTR => Syscall::Getxattr,
        aarch64::LGETXATTR => Syscall::Lgetxattr,
        aarch64::FGETXATTR => Syscall::Fgetxattr,
        aarch64::LISTXATTR => Syscall::Listxattr,
        aarch64::LLISTXATTR => Syscall::Llistxattr,
        aarch64::FLISTXATTR => Syscall::Flistxattr,
        aarch64::REMOVEXATTR => Syscall::Removexattr,
        aarch64::LREMOVEXATTR => Syscall::Lremovexattr,
        aarch64::FREMOVEXATTR => Syscall::Fremovexattr,
        aarch64::PIVOT_ROOT => Syscall::PivotRoot,
        aarch64::FALLOCATE => Syscall::Fallocate,
        aarch64::CHROOT => Syscall::Chroot,
        aarch64::SYNCFS => Syscall::Syncfs,
        _ => return None,
    };
    Some(call)
}

/// AArch64 sockets and System V IPC, wherever they sit in the table.
fn aarch64_sockets_and_shm(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::SHMGET => Syscall::Shmget,
        aarch64::SHMCTL => Syscall::Shmctl,
        aarch64::SHMAT => Syscall::Shmat,
        aarch64::SHMDT => Syscall::Shmdt,
        aarch64::SOCKET => Syscall::Socket,
        aarch64::SOCKETPAIR => Syscall::Socketpair,
        aarch64::BIND => Syscall::Bind,
        aarch64::LISTEN => Syscall::Listen,
        aarch64::ACCEPT => Syscall::Accept,
        aarch64::CONNECT => Syscall::Connect,
        aarch64::GETSOCKNAME => Syscall::Getsockname,
        aarch64::GETPEERNAME => Syscall::Getpeername,
        aarch64::SENDTO => Syscall::Sendto,
        aarch64::RECVFROM => Syscall::Recvfrom,
        aarch64::SETSOCKOPT => Syscall::Setsockopt,
        aarch64::GETSOCKOPT => Syscall::Getsockopt,
        aarch64::SHUTDOWN => Syscall::Shutdown,
        aarch64::SENDMSG => Syscall::Sendmsg,
        aarch64::RECVMSG => Syscall::Recvmsg,
        aarch64::ACCEPT4 => Syscall::Accept4,
        aarch64::MSGGET => Syscall::Msgget,
        aarch64::MSGSND => Syscall::Msgsnd,
        aarch64::MSGRCV => Syscall::Msgrcv,
        aarch64::MSGCTL => Syscall::Msgctl,
        aarch64::SEMGET => Syscall::Semget,
        aarch64::SEMOP => Syscall::Semop,
        aarch64::SEMCTL => Syscall::Semctl,
        aarch64::SEMTIMEDOP => Syscall::Semtimedop,
        _ => return None,
    };
    Some(call)
}

/// AArch64 credential changes, capabilities, process groups and sessions.
fn aarch64_credentials_and_sessions(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::CAPGET => Syscall::Capget,
        aarch64::CAPSET => Syscall::Capset,
        aarch64::SETREGID => Syscall::Setregid,
        aarch64::SETREUID => Syscall::Setreuid,
        aarch64::SETRESUID => Syscall::Setresuid,
        aarch64::SETRESGID => Syscall::Setresgid,
        aarch64::SETFSUID => Syscall::Setfsuid,
        aarch64::SETFSGID => Syscall::Setfsgid,
        aarch64::GETSID => Syscall::Getsid,
        aarch64::SETSID => Syscall::Setsid,
        aarch64::SETGROUPS => Syscall::Setgroups,
        _ => return None,
    };
    Some(call)
}

/// AArch64 host names, kernel modules, swap, namespaces and accounting.
fn aarch64_administration(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::VHANGUP => Syscall::Vhangup,
        aarch64::ACCT => Syscall::Acct,
        aarch64::PERSONALITY => Syscall::Personality,
        aarch64::INIT_MODULE => Syscall::InitModule,
        aarch64::DELETE_MODULE => Syscall::DeleteModule,
        aarch64::SYSLOG => Syscall::Syslog,
        aarch64::REBOOT => Syscall::Reboot,
        aarch64::SETHOSTNAME => Syscall::Sethostname,
        aarch64::SETDOMAINNAME => Syscall::Setdomainname,
        aarch64::SWAPON => Syscall::Swapon,
        aarch64::SWAPOFF => Syscall::Swapoff,
        aarch64::SETNS => Syscall::Setns,
        aarch64::FINIT_MODULE => Syscall::FinitModule,
        _ => return None,
    };
    Some(call)
}

/// AArch64 clock setting and tuning, and interval timers.
fn aarch64_clocks_and_timers(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::GETITIMER => Syscall::Getitimer,
        aarch64::SETITIMER => Syscall::Setitimer,
        aarch64::CLOCK_SETTIME => Syscall::ClockSettime,
        aarch64::TIMES => Syscall::Times,
        aarch64::SETTIMEOFDAY => Syscall::Settimeofday,
        aarch64::ADJTIMEX => Syscall::Adjtimex,
        aarch64::CLOCK_ADJTIME => Syscall::ClockAdjtime,
        aarch64::TIMERFD_CREATE => Syscall::TimerfdCreate,
        aarch64::TIMERFD_SETTIME => Syscall::TimerfdSettime,
        aarch64::TIMERFD_GETTIME => Syscall::TimerfdGettime,
        _ => return None,
    };
    Some(call)
}

/// AArch64 descriptor and signal waits, and scheduling priorities.
fn aarch64_signals_and_scheduling(nr: usize) -> Option<Syscall> {
    let call = match nr {
        aarch64::IOPRIO_SET => Syscall::IoprioSet,
        aarch64::IOPRIO_GET => Syscall::IoprioGet,
        aarch64::PSELECT6 => Syscall::Pselect6,
        aarch64::SCHED_SETPARAM => Syscall::SchedSetparam,
        aarch64::SCHED_GET_PRIORITY_MAX => Syscall::SchedGetPriorityMax,
        aarch64::SCHED_GET_PRIORITY_MIN => Syscall::SchedGetPriorityMin,
        aarch64::SCHED_RR_GET_INTERVAL => Syscall::SchedRrGetInterval,
        aarch64::RT_SIGPENDING => Syscall::RtSigpending,
        aarch64::RT_SIGTIMEDWAIT => Syscall::RtSigtimedwait,
        aarch64::RT_SIGQUEUEINFO => Syscall::RtSigqueueinfo,
        aarch64::SETPRIORITY => Syscall::Setpriority,
        aarch64::GETPRIORITY => Syscall::Getpriority,
        aarch64::GETCPU => Syscall::Getcpu,
        _ => return None,
    };
    Some(call)
}

/// Translate a 32-bit ARMv7-A (EABI) system call number.
///
/// Returns [`None`] for a number this crate does not know, which the caller
/// reports as `ENOSYS`.
#[must_use]
pub fn from_arm(nr: usize) -> Option<Syscall> {
    // Asked in turn, for the reason given on `from_aarch64`.
    arm_early(nr)
        .or_else(|| arm_signals_and_mm(nr))
        .or_else(|| arm_ids_and_at_family(nr))
        .or_else(|| arm_recent(nr))
        .or_else(|| arm_sockets_and_shm(nr))
        .or_else(|| arm_credentials_and_sessions(nr))
        .or_else(|| arm_administration(nr))
        .or_else(|| arm_clocks_and_timers(nr))
        .or_else(|| arm_signals_and_scheduling(nr))
}

/// ARMv7-A numbers 0 to 99: the calls inherited from the very first Linux/ARM
/// table -- process lifetime, descriptors and paths.
fn arm_early(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::RESTART_SYSCALL => Syscall::RestartSyscall,
        arm::EXIT => Syscall::Exit,
        arm::FORK => Syscall::Fork,
        arm::READ => Syscall::Read,
        arm::WRITE => Syscall::Write,
        arm::OPEN => Syscall::Open,
        arm::CREAT => Syscall::Creat,
        arm::CLOSE => Syscall::Close,
        arm::LINK => Syscall::Link,
        arm::UNLINK => Syscall::Unlink,
        arm::EXECVE => Syscall::Execve,
        arm::CHDIR => Syscall::Chdir,
        arm::MKNOD => Syscall::Mknod,
        arm::CHMOD => Syscall::Chmod,
        arm::LSEEK => Syscall::Lseek,
        arm::GETPID => Syscall::Getpid,
        arm::MOUNT => Syscall::Mount,
        arm::ACCESS => Syscall::Access,
        arm::SYNC => Syscall::Sync,
        arm::KILL => Syscall::Kill,
        arm::RENAME => Syscall::Rename,
        arm::MKDIR => Syscall::Mkdir,
        arm::RMDIR => Syscall::Rmdir,
        arm::DUP => Syscall::Dup,
        arm::PIPE => Syscall::Pipe,
        arm::BRK => Syscall::Brk,
        arm::UMOUNT2 => Syscall::Umount2,
        arm::IOCTL => Syscall::Ioctl,
        arm::FCNTL => Syscall::Fcntl,
        arm::SETPGID => Syscall::Setpgid,
        arm::UMASK => Syscall::Umask,
        arm::DUP2 => Syscall::Dup2,
        arm::GETPPID => Syscall::Getppid,
        arm::SETRLIMIT => Syscall::Setrlimit,
        arm::GETRUSAGE => Syscall::Getrusage,
        arm::GETTIMEOFDAY => Syscall::Gettimeofday,
        arm::SYMLINK => Syscall::Symlink,
        arm::READLINK => Syscall::Readlink,
        arm::MUNMAP => Syscall::Munmap,
        arm::TRUNCATE => Syscall::Truncate,
        arm::FTRUNCATE => Syscall::Ftruncate,
        arm::FCHMOD => Syscall::Fchmod,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A numbers 100 to 199: signals, memory, scheduling, and the `64`
/// forms that replaced calls a 32-bit `off_t` had outgrown.
fn arm_signals_and_mm(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::WAIT4 => Syscall::Wait4,
        arm::SYSINFO => Syscall::Sysinfo,
        arm::FSYNC => Syscall::Fsync,
        arm::SIGRETURN => Syscall::Sigreturn,
        arm::CLONE => Syscall::Clone,
        arm::UNAME => Syscall::Uname,
        arm::MPROTECT => Syscall::Mprotect,
        arm::GETPGID => Syscall::Getpgid,
        arm::FCHDIR => Syscall::Fchdir,
        arm::LLSEEK => Syscall::Llseek,
        arm::MSYNC => Syscall::Msync,
        arm::READV => Syscall::Readv,
        arm::WRITEV => Syscall::Writev,
        arm::FDATASYNC => Syscall::Fdatasync,
        arm::SCHED_GETPARAM => Syscall::SchedGetparam,
        arm::SCHED_SETSCHEDULER => Syscall::SchedSetscheduler,
        arm::SCHED_GETSCHEDULER => Syscall::SchedGetscheduler,
        arm::SCHED_YIELD => Syscall::SchedYield,
        arm::NANOSLEEP => Syscall::Nanosleep,
        arm::MREMAP => Syscall::Mremap,
        arm::POLL => Syscall::Poll,
        arm::PRCTL => Syscall::Prctl,
        arm::RT_SIGRETURN => Syscall::RtSigreturn,
        arm::RT_SIGACTION => Syscall::RtSigaction,
        arm::RT_SIGPROCMASK => Syscall::RtSigprocmask,
        arm::RT_SIGSUSPEND => Syscall::RtSigsuspend,
        arm::PREAD64 => Syscall::Pread64,
        arm::PWRITE64 => Syscall::Pwrite64,
        arm::GETCWD => Syscall::Getcwd,
        arm::SIGALTSTACK => Syscall::Sigaltstack,
        arm::VFORK => Syscall::Vfork,
        arm::UGETRLIMIT => Syscall::Getrlimit,
        arm::MMAP2 => Syscall::Mmap2,
        arm::TRUNCATE64 => Syscall::Truncate64,
        arm::FTRUNCATE64 => Syscall::Ftruncate64,
        arm::STAT64 => Syscall::Stat64,
        arm::LSTAT64 => Syscall::Lstat64,
        arm::FSTAT64 => Syscall::Fstat64,
        arm::LCHOWN32 => Syscall::Lchown,
        arm::GETUID32 => Syscall::Getuid,
        arm::FLOCK => Syscall::Flock,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A numbers 200 to 329: the 32-bit credential calls, futexes, and the
/// `*at` family.
fn arm_ids_and_at_family(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::GETGID32 => Syscall::Getgid,
        arm::GETEUID32 => Syscall::Geteuid,
        arm::GETEGID32 => Syscall::Getegid,
        arm::GETGROUPS32 => Syscall::Getgroups,
        arm::FCHOWN32 => Syscall::Fchown,
        arm::GETRESUID32 => Syscall::Getresuid,
        arm::GETRESGID32 => Syscall::Getresgid,
        arm::CHOWN32 => Syscall::Chown,
        arm::SETUID32 => Syscall::Setuid,
        arm::SETGID32 => Syscall::Setgid,
        arm::GETDENTS64 => Syscall::Getdents64,
        arm::MADVISE => Syscall::Madvise,
        arm::MINCORE => Syscall::Mincore,
        arm::FCNTL64 => Syscall::Fcntl64,
        arm::GETTID => Syscall::Gettid,
        arm::TKILL => Syscall::Tkill,
        arm::SENDFILE64 => Syscall::Sendfile64,
        arm::SPLICE => Syscall::Splice,
        arm::COPY_FILE_RANGE => Syscall::CopyFileRange,
        arm::FUTEX => Syscall::Futex,
        arm::SCHED_SETAFFINITY => Syscall::SchedSetaffinity,
        arm::SCHED_GETAFFINITY => Syscall::SchedGetaffinity,
        arm::EXIT_GROUP => Syscall::ExitGroup,
        arm::EPOLL_CREATE => Syscall::EpollCreate,
        arm::EPOLL_CTL => Syscall::EpollCtl,
        arm::EPOLL_WAIT => Syscall::EpollWait,
        arm::SET_TID_ADDRESS => Syscall::SetTidAddress,
        arm::CLOCK_GETTIME => Syscall::ClockGettime,
        arm::CLOCK_GETRES => Syscall::ClockGetres,
        arm::CLOCK_NANOSLEEP => Syscall::ClockNanosleep,
        arm::STATFS64 => Syscall::Statfs64,
        arm::FSTATFS64 => Syscall::Fstatfs64,
        arm::TGKILL => Syscall::Tgkill,
        arm::WAITID => Syscall::Waitid,
        arm::OPENAT => Syscall::Openat,
        arm::MKDIRAT => Syscall::Mkdirat,
        arm::MKNODAT => Syscall::Mknodat,
        arm::FCHOWNAT => Syscall::Fchownat,
        arm::FSTATAT64 => Syscall::Fstatat64,
        arm::UNLINKAT => Syscall::Unlinkat,
        arm::RENAMEAT => Syscall::Renameat,
        arm::READAHEAD => Syscall::Readahead,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A numbers 330 and up, and the ARM-private range: everything added
/// after the `*at` family, including the time64 calls a current musl actually
/// issues.
fn arm_recent(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::LINKAT => Syscall::Linkat,
        arm::SYMLINKAT => Syscall::Symlinkat,
        arm::READLINKAT => Syscall::Readlinkat,
        arm::FCHMODAT => Syscall::Fchmodat,
        arm::FACCESSAT => Syscall::Faccessat,
        arm::PPOLL => Syscall::Ppoll,
        arm::UNSHARE => Syscall::Unshare,
        arm::SET_ROBUST_LIST => Syscall::SetRobustList,
        arm::GET_ROBUST_LIST => Syscall::GetRobustList,
        arm::EPOLL_PWAIT => Syscall::EpollPwait,
        arm::UTIMENSAT => Syscall::Utimensat,
        arm::EVENTFD => Syscall::Eventfd,
        arm::EVENTFD2 => Syscall::Eventfd2,
        arm::SIGNALFD => Syscall::Signalfd,
        arm::SIGNALFD4 => Syscall::Signalfd4,
        arm::EPOLL_CREATE1 => Syscall::EpollCreate1,
        arm::DUP3 => Syscall::Dup3,
        arm::PIPE2 => Syscall::Pipe2,
        arm::PRLIMIT64 => Syscall::Prlimit64,
        arm::NAME_TO_HANDLE_AT => Syscall::NameToHandleAt,
        arm::RENAMEAT2 => Syscall::Renameat2,
        arm::SECCOMP => Syscall::Seccomp,
        arm::GETRANDOM => Syscall::Getrandom,
        arm::MEMFD_CREATE => Syscall::MemfdCreate,
        arm::EXECVEAT => Syscall::Execveat,
        arm::MEMBARRIER => Syscall::Membarrier,
        arm::STATX => Syscall::Statx,
        arm::RSEQ => Syscall::Rseq,
        arm::CLOCK_GETTIME64 => Syscall::ClockGettime64,
        arm::CLOCK_NANOSLEEP_TIME64 => Syscall::ClockNanosleepTime64,
        arm::UTIMENSAT_TIME64 => Syscall::UtimensatTime64,
        arm::PPOLL_TIME64 => Syscall::PpollTime64,
        arm::FUTEX_TIME64 => Syscall::FutexTime64,
        arm::INOTIFY_INIT => Syscall::InotifyInit,
        arm::INOTIFY_INIT1 => Syscall::InotifyInit1,
        arm::INOTIFY_ADD_WATCH => Syscall::InotifyAddWatch,
        arm::INOTIFY_RM_WATCH => Syscall::InotifyRmWatch,
        arm::PIDFD_OPEN => Syscall::PidfdOpen,
        arm::PIDFD_SEND_SIGNAL => Syscall::PidfdSendSignal,
        arm::CLONE3 => Syscall::Clone3,
        arm::OPENAT2 => Syscall::Openat2,
        arm::FACCESSAT2 => Syscall::Faccessat2,
        arm::EPOLL_PWAIT2 => Syscall::EpollPwait2,
        arm::SETXATTR => Syscall::Setxattr,
        arm::LSETXATTR => Syscall::Lsetxattr,
        arm::FSETXATTR => Syscall::Fsetxattr,
        arm::GETXATTR => Syscall::Getxattr,
        arm::LGETXATTR => Syscall::Lgetxattr,
        arm::FGETXATTR => Syscall::Fgetxattr,
        arm::LISTXATTR => Syscall::Listxattr,
        arm::LLISTXATTR => Syscall::Llistxattr,
        arm::FLISTXATTR => Syscall::Flistxattr,
        arm::REMOVEXATTR => Syscall::Removexattr,
        arm::LREMOVEXATTR => Syscall::Lremovexattr,
        arm::FREMOVEXATTR => Syscall::Fremovexattr,
        arm::PIVOT_ROOT => Syscall::PivotRoot,
        arm::FALLOCATE => Syscall::Fallocate,
        arm::CHROOT => Syscall::Chroot,
        arm::SYNCFS => Syscall::Syncfs,
        arm::ARM_CACHEFLUSH => Syscall::ArmCacheflush,
        arm::ARM_SET_TLS => Syscall::ArmSetTls,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A sockets and System V IPC, wherever they sit in the table.
fn arm_sockets_and_shm(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::SOCKET => Syscall::Socket,
        arm::BIND => Syscall::Bind,
        arm::CONNECT => Syscall::Connect,
        arm::LISTEN => Syscall::Listen,
        arm::ACCEPT => Syscall::Accept,
        arm::GETSOCKNAME => Syscall::Getsockname,
        arm::GETPEERNAME => Syscall::Getpeername,
        arm::SOCKETPAIR => Syscall::Socketpair,
        arm::SENDTO => Syscall::Sendto,
        arm::RECVFROM => Syscall::Recvfrom,
        arm::SHUTDOWN => Syscall::Shutdown,
        arm::SETSOCKOPT => Syscall::Setsockopt,
        arm::GETSOCKOPT => Syscall::Getsockopt,
        arm::SENDMSG => Syscall::Sendmsg,
        arm::RECVMSG => Syscall::Recvmsg,
        arm::SHMAT => Syscall::Shmat,
        arm::SHMDT => Syscall::Shmdt,
        arm::SHMGET => Syscall::Shmget,
        arm::SHMCTL => Syscall::Shmctl,
        arm::ACCEPT4 => Syscall::Accept4,
        arm::MSGGET => Syscall::Msgget,
        arm::MSGSND => Syscall::Msgsnd,
        arm::MSGRCV => Syscall::Msgrcv,
        arm::MSGCTL => Syscall::Msgctl,
        arm::SEMGET => Syscall::Semget,
        arm::SEMOP => Syscall::Semop,
        arm::SEMCTL => Syscall::Semctl,
        arm::SEMTIMEDOP => Syscall::Semtimedop,
        arm::SEMTIMEDOP_TIME64 => Syscall::SemtimedopTime64,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A credential changes, capabilities, process groups and sessions.
fn arm_credentials_and_sessions(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::GETPGRP => Syscall::Getpgrp,
        arm::SETSID => Syscall::Setsid,
        arm::GETSID => Syscall::Getsid,
        arm::CAPGET => Syscall::Capget,
        arm::CAPSET => Syscall::Capset,
        arm::SETREUID32 => Syscall::Setreuid,
        arm::SETREGID32 => Syscall::Setregid,
        arm::SETGROUPS32 => Syscall::Setgroups,
        arm::SETRESUID32 => Syscall::Setresuid,
        arm::SETRESGID32 => Syscall::Setresgid,
        arm::SETFSUID32 => Syscall::Setfsuid,
        arm::SETFSGID32 => Syscall::Setfsgid,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A host names, kernel modules, swap, namespaces and accounting.
fn arm_administration(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::ACCT => Syscall::Acct,
        arm::SETHOSTNAME => Syscall::Sethostname,
        arm::SWAPON => Syscall::Swapon,
        arm::REBOOT => Syscall::Reboot,
        arm::SYSLOG => Syscall::Syslog,
        arm::VHANGUP => Syscall::Vhangup,
        arm::SWAPOFF => Syscall::Swapoff,
        arm::SETDOMAINNAME => Syscall::Setdomainname,
        arm::INIT_MODULE => Syscall::InitModule,
        arm::DELETE_MODULE => Syscall::DeleteModule,
        arm::PERSONALITY => Syscall::Personality,
        arm::SETNS => Syscall::Setns,
        arm::FINIT_MODULE => Syscall::FinitModule,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A clock setting and tuning, and interval timers, time64 forms included.
fn arm_clocks_and_timers(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::TIMES => Syscall::Times,
        arm::SETTIMEOFDAY => Syscall::Settimeofday,
        arm::SETITIMER => Syscall::Setitimer,
        arm::GETITIMER => Syscall::Getitimer,
        arm::ADJTIMEX => Syscall::Adjtimex,
        arm::CLOCK_SETTIME => Syscall::ClockSettime,
        arm::CLOCK_ADJTIME => Syscall::ClockAdjtime,
        arm::CLOCK_SETTIME64 => Syscall::ClockSettime64,
        arm::CLOCK_ADJTIME64 => Syscall::ClockAdjtime64,
        arm::CLOCK_GETRES_TIME64 => Syscall::ClockGetresTime64,
        arm::TIMERFD_CREATE => Syscall::TimerfdCreate,
        arm::TIMERFD_SETTIME => Syscall::TimerfdSettime,
        arm::TIMERFD_GETTIME => Syscall::TimerfdGettime,
        arm::TIMERFD_SETTIME64 => Syscall::TimerfdSettime64,
        arm::TIMERFD_GETTIME64 => Syscall::TimerfdGettime64,
        _ => return None,
    };
    Some(call)
}

/// ARMv7-A descriptor and signal waits, and scheduling priorities, time64 forms included.
fn arm_signals_and_scheduling(nr: usize) -> Option<Syscall> {
    let call = match nr {
        arm::PAUSE => Syscall::Pause,
        arm::GETPRIORITY => Syscall::Getpriority,
        arm::SETPRIORITY => Syscall::Setpriority,
        arm::NEWSELECT => Syscall::Select,
        arm::SCHED_SETPARAM => Syscall::SchedSetparam,
        arm::SCHED_GET_PRIORITY_MAX => Syscall::SchedGetPriorityMax,
        arm::SCHED_GET_PRIORITY_MIN => Syscall::SchedGetPriorityMin,
        arm::SCHED_RR_GET_INTERVAL => Syscall::SchedRrGetInterval,
        arm::RT_SIGPENDING => Syscall::RtSigpending,
        arm::RT_SIGTIMEDWAIT => Syscall::RtSigtimedwait,
        arm::RT_SIGQUEUEINFO => Syscall::RtSigqueueinfo,
        arm::IOPRIO_SET => Syscall::IoprioSet,
        arm::IOPRIO_GET => Syscall::IoprioGet,
        arm::PSELECT6 => Syscall::Pselect6,
        arm::GETCPU => Syscall::Getcpu,
        arm::PSELECT6_TIME64 => Syscall::Pselect6Time64,
        arm::RT_SIGTIMEDWAIT_TIME64 => Syscall::RtSigtimedwaitTime64,
        arm::SCHED_RR_GET_INTERVAL_TIME64 => Syscall::SchedRrGetIntervalTime64,
        _ => return None,
    };
    Some(call)
}

/// Translate an i386 system call number: a 32-bit program's, or any
/// program's `int $0x80`.
///
/// Returns [`None`] for a number this crate does not translate, which the
/// caller reports as `ENOSYS`. That includes most of the table, on purpose:
/// the kernel is 64-bit and the caller is not, so a handler that reads a
/// `long`, a pointer inside a structure or a 64-bit register pair in its own
/// widths would misread an i386 call silently. A call is mapped here only
/// once its handler has been read for that (`docs/I386.md` §3.2).
#[must_use]
pub fn from_i386(nr: usize) -> Option<Syscall> {
    // Asked in turn, for the reason given on `from_aarch64`.
    i386_early(nr)
        .or_else(|| i386_middle(nr))
        .or_else(|| i386_ids_and_at_family(nr))
        .or_else(|| i386_recent(nr))
}

/// i386 numbers 0 to 99: the calls inherited from the first Linux table --
/// process lifetime, descriptors and paths.
fn i386_early(nr: usize) -> Option<Syscall> {
    let call = match nr {
        i386::RESTART_SYSCALL => Syscall::RestartSyscall,
        i386::EXIT => Syscall::Exit,
        i386::FORK => Syscall::Fork,
        i386::READ => Syscall::Read,
        i386::WRITE => Syscall::Write,
        i386::OPEN => Syscall::Open,
        i386::CLOSE => Syscall::Close,
        i386::CREAT => Syscall::Creat,
        i386::LINK => Syscall::Link,
        i386::UNLINK => Syscall::Unlink,
        i386::EXECVE => Syscall::Execve,
        i386::CHDIR => Syscall::Chdir,
        i386::MKNOD => Syscall::Mknod,
        i386::CHMOD => Syscall::Chmod,
        i386::LSEEK => Syscall::Lseek,
        i386::GETPID => Syscall::Getpid,
        i386::MOUNT => Syscall::Mount,
        i386::PAUSE => Syscall::Pause,
        i386::ACCESS => Syscall::Access,
        i386::SYNC => Syscall::Sync,
        i386::KILL => Syscall::Kill,
        i386::RENAME => Syscall::Rename,
        i386::MKDIR => Syscall::Mkdir,
        i386::RMDIR => Syscall::Rmdir,
        i386::DUP => Syscall::Dup,
        i386::PIPE => Syscall::Pipe,
        i386::TIMES => Syscall::Times,
        i386::BRK => Syscall::Brk,
        i386::UMOUNT2 => Syscall::Umount2,
        i386::IOCTL => Syscall::Ioctl,
        i386::FCNTL => Syscall::Fcntl,
        i386::SETPGID => Syscall::Setpgid,
        i386::UMASK => Syscall::Umask,
        i386::CHROOT => Syscall::Chroot,
        i386::DUP2 => Syscall::Dup2,
        i386::GETPPID => Syscall::Getppid,
        i386::GETPGRP => Syscall::Getpgrp,
        i386::SETSID => Syscall::Setsid,
        i386::SETHOSTNAME => Syscall::Sethostname,
        i386::GETRUSAGE => Syscall::Getrusage,
        i386::SYMLINK => Syscall::Symlink,
        i386::READLINK => Syscall::Readlink,
        i386::REBOOT => Syscall::Reboot,
        i386::MUNMAP => Syscall::Munmap,
        i386::TRUNCATE => Syscall::Truncate,
        i386::FTRUNCATE => Syscall::Ftruncate,
        i386::FCHMOD => Syscall::Fchmod,
        i386::GETPRIORITY => Syscall::Getpriority,
        i386::SETPRIORITY => Syscall::Setpriority,
        i386::GETTIMEOFDAY => Syscall::Gettimeofday,
        i386::SETTIMEOFDAY => Syscall::Settimeofday,
        i386::TIME => Syscall::Time,
        i386::ALARM => Syscall::Alarm,
        i386::SETRLIMIT => Syscall::Setrlimit,
        _ => return None,
    };
    Some(call)
}

/// i386 numbers 100 to 199: signals, memory, scheduling and the 64-bit
/// file offsets.
fn i386_middle(nr: usize) -> Option<Syscall> {
    let call = match nr {
        i386::SYSLOG => Syscall::Syslog,
        i386::WAIT4 => Syscall::Wait4,
        i386::SYSINFO => Syscall::Sysinfo,
        i386::FSYNC => Syscall::Fsync,
        i386::SIGRETURN => Syscall::Sigreturn,
        i386::CLONE => Syscall::Clone,
        i386::SETDOMAINNAME => Syscall::Setdomainname,
        i386::UNAME => Syscall::Uname,
        i386::MPROTECT => Syscall::Mprotect,
        i386::GETPGID => Syscall::Getpgid,
        i386::FCHDIR => Syscall::Fchdir,
        i386::LLSEEK => Syscall::Llseek,
        i386::FLOCK => Syscall::Flock,
        i386::MSYNC => Syscall::Msync,
        i386::READV => Syscall::Readv,
        i386::WRITEV => Syscall::Writev,
        i386::GETSID => Syscall::Getsid,
        i386::FDATASYNC => Syscall::Fdatasync,
        i386::SCHED_SETPARAM => Syscall::SchedSetparam,
        i386::SCHED_GETPARAM => Syscall::SchedGetparam,
        i386::SCHED_SETSCHEDULER => Syscall::SchedSetscheduler,
        i386::SCHED_GETSCHEDULER => Syscall::SchedGetscheduler,
        i386::SCHED_YIELD => Syscall::SchedYield,
        i386::SCHED_GET_PRIORITY_MAX => Syscall::SchedGetPriorityMax,
        i386::SCHED_GET_PRIORITY_MIN => Syscall::SchedGetPriorityMin,
        i386::MREMAP => Syscall::Mremap,
        i386::POLL => Syscall::Poll,
        i386::PRCTL => Syscall::Prctl,
        i386::RT_SIGRETURN => Syscall::RtSigreturn,
        i386::RT_SIGACTION => Syscall::RtSigaction,
        i386::RT_SIGPROCMASK => Syscall::RtSigprocmask,
        i386::RT_SIGPENDING => Syscall::RtSigpending,
        i386::RT_SIGSUSPEND => Syscall::RtSigsuspend,
        i386::PREAD64 => Syscall::Pread64,
        i386::PWRITE64 => Syscall::Pwrite64,
        i386::GETCWD => Syscall::Getcwd,
        i386::CAPGET => Syscall::Capget,
        i386::CAPSET => Syscall::Capset,
        i386::SIGALTSTACK => Syscall::Sigaltstack,
        i386::VFORK => Syscall::Vfork,
        i386::MMAP2 => Syscall::Mmap2,
        i386::TRUNCATE64 => Syscall::Truncate64,
        i386::FTRUNCATE64 => Syscall::Ftruncate64,
        i386::LCHOWN32 => Syscall::Lchown,
        i386::GETUID32 => Syscall::Getuid,
        i386::NANOSLEEP => Syscall::Nanosleep,
        i386::NEWSELECT => Syscall::Select,
        i386::ADJTIMEX => Syscall::Adjtimex,
        i386::SOCKETCALL => Syscall::Socketcall,
        i386::IPC => Syscall::Ipc,
        i386::UGETRLIMIT => Syscall::Getrlimit,
        i386::STAT64 => Syscall::Stat64,
        i386::LSTAT64 => Syscall::Lstat64,
        i386::FSTAT64 => Syscall::Fstat64,
        _ => return None,
    };
    Some(call)
}

/// i386 numbers 200 to 299: the `32` credential forms, threads, epoll and
/// the clocks.
fn i386_ids_and_at_family(nr: usize) -> Option<Syscall> {
    let call = match nr {
        i386::GETGID32 => Syscall::Getgid,
        i386::GETEUID32 => Syscall::Geteuid,
        i386::GETEGID32 => Syscall::Getegid,
        i386::SETREUID32 => Syscall::Setreuid,
        i386::SETREGID32 => Syscall::Setregid,
        i386::GETGROUPS32 => Syscall::Getgroups,
        i386::SETGROUPS32 => Syscall::Setgroups,
        i386::FCHOWN32 => Syscall::Fchown,
        i386::SETRESUID32 => Syscall::Setresuid,
        i386::GETRESUID32 => Syscall::Getresuid,
        i386::SETRESGID32 => Syscall::Setresgid,
        i386::GETRESGID32 => Syscall::Getresgid,
        i386::CHOWN32 => Syscall::Chown,
        i386::SETUID32 => Syscall::Setuid,
        i386::SETGID32 => Syscall::Setgid,
        i386::SETFSUID32 => Syscall::Setfsuid,
        i386::SETFSGID32 => Syscall::Setfsgid,
        i386::PIVOT_ROOT => Syscall::PivotRoot,
        i386::MADVISE => Syscall::Madvise,
        i386::MINCORE => Syscall::Mincore,
        i386::GETDENTS64 => Syscall::Getdents64,
        i386::FCNTL64 => Syscall::Fcntl64,
        i386::GETTID => Syscall::Gettid,
        i386::READAHEAD => Syscall::Readahead,
        i386::SETXATTR => Syscall::Setxattr,
        i386::LSETXATTR => Syscall::Lsetxattr,
        i386::FSETXATTR => Syscall::Fsetxattr,
        i386::GETXATTR => Syscall::Getxattr,
        i386::LGETXATTR => Syscall::Lgetxattr,
        i386::FGETXATTR => Syscall::Fgetxattr,
        i386::LISTXATTR => Syscall::Listxattr,
        i386::LLISTXATTR => Syscall::Llistxattr,
        i386::FLISTXATTR => Syscall::Flistxattr,
        i386::REMOVEXATTR => Syscall::Removexattr,
        i386::LREMOVEXATTR => Syscall::Lremovexattr,
        i386::FREMOVEXATTR => Syscall::Fremovexattr,
        i386::TKILL => Syscall::Tkill,
        i386::SENDFILE64 => Syscall::Sendfile64,
        i386::SCHED_SETAFFINITY => Syscall::SchedSetaffinity,
        i386::SCHED_GETAFFINITY => Syscall::SchedGetaffinity,
        i386::SET_THREAD_AREA => Syscall::SetThreadArea,
        i386::GET_THREAD_AREA => Syscall::GetThreadArea,
        i386::EXIT_GROUP => Syscall::ExitGroup,
        i386::EPOLL_CREATE => Syscall::EpollCreate,
        i386::EPOLL_CTL => Syscall::EpollCtl,
        i386::EPOLL_WAIT => Syscall::EpollWait,
        i386::SET_TID_ADDRESS => Syscall::SetTidAddress,
        i386::STATFS64 => Syscall::Statfs64,
        i386::FSTATFS64 => Syscall::Fstatfs64,
        i386::TGKILL => Syscall::Tgkill,
        i386::INOTIFY_INIT => Syscall::InotifyInit,
        i386::INOTIFY_ADD_WATCH => Syscall::InotifyAddWatch,
        i386::INOTIFY_RM_WATCH => Syscall::InotifyRmWatch,
        i386::OPENAT => Syscall::Openat,
        i386::MKDIRAT => Syscall::Mkdirat,
        i386::MKNODAT => Syscall::Mknodat,
        i386::FCHOWNAT => Syscall::Fchownat,
        i386::CLOCK_NANOSLEEP => Syscall::ClockNanosleep,
        i386::FUTEX => Syscall::Futex,
        i386::CLOCK_GETTIME => Syscall::ClockGettime,
        i386::CLOCK_GETRES => Syscall::ClockGetres,
        i386::CLOCK_SETTIME => Syscall::ClockSettime,
        i386::WAITID => Syscall::Waitid,
        _ => return None,
    };
    Some(call)
}

/// i386 numbers 300 and up: the `at` family, the descriptor-making calls
/// and the time64 set.
fn i386_recent(nr: usize) -> Option<Syscall> {
    let call = match nr {
        i386::UNLINKAT => Syscall::Unlinkat,
        i386::RENAMEAT => Syscall::Renameat,
        i386::LINKAT => Syscall::Linkat,
        i386::SYMLINKAT => Syscall::Symlinkat,
        i386::READLINKAT => Syscall::Readlinkat,
        i386::FCHMODAT => Syscall::Fchmodat,
        i386::FACCESSAT => Syscall::Faccessat,
        i386::UNSHARE => Syscall::Unshare,
        i386::SPLICE => Syscall::Splice,
        i386::GETCPU => Syscall::Getcpu,
        i386::EPOLL_PWAIT => Syscall::EpollPwait,
        i386::SIGNALFD => Syscall::Signalfd,
        i386::TIMERFD_CREATE => Syscall::TimerfdCreate,
        i386::EVENTFD => Syscall::Eventfd,
        i386::FALLOCATE => Syscall::Fallocate,
        i386::SIGNALFD4 => Syscall::Signalfd4,
        i386::EVENTFD2 => Syscall::Eventfd2,
        i386::EPOLL_CREATE1 => Syscall::EpollCreate1,
        i386::DUP3 => Syscall::Dup3,
        i386::PIPE2 => Syscall::Pipe2,
        i386::INOTIFY_INIT1 => Syscall::InotifyInit1,
        i386::PRLIMIT64 => Syscall::Prlimit64,
        i386::NAME_TO_HANDLE_AT => Syscall::NameToHandleAt,
        i386::SYNCFS => Syscall::Syncfs,
        i386::SETNS => Syscall::Setns,
        i386::RENAMEAT2 => Syscall::Renameat2,
        i386::SECCOMP => Syscall::Seccomp,
        i386::GETRANDOM => Syscall::Getrandom,
        i386::MEMFD_CREATE => Syscall::MemfdCreate,
        i386::EXECVEAT => Syscall::Execveat,
        i386::MEMBARRIER => Syscall::Membarrier,
        i386::COPY_FILE_RANGE => Syscall::CopyFileRange,
        i386::STATX => Syscall::Statx,
        i386::CLOCK_GETTIME64 => Syscall::ClockGettime64,
        i386::CLOCK_SETTIME64 => Syscall::ClockSettime64,
        i386::CLOCK_GETRES_TIME64 => Syscall::ClockGetresTime64,
        i386::CLOCK_NANOSLEEP_TIME64 => Syscall::ClockNanosleepTime64,
        i386::TIMERFD_GETTIME64 => Syscall::TimerfdGettime64,
        i386::TIMERFD_SETTIME64 => Syscall::TimerfdSettime64,
        i386::UTIMENSAT_TIME64 => Syscall::UtimensatTime64,
        i386::PSELECT6_TIME64 => Syscall::Pselect6Time64,
        i386::PPOLL_TIME64 => Syscall::PpollTime64,
        i386::FUTEX_TIME64 => Syscall::FutexTime64,
        i386::SEMGET => Syscall::Semget,
        i386::SEMCTL => Syscall::Semctl,
        i386::SEMTIMEDOP_TIME64 => Syscall::SemtimedopTime64,
        i386::SHMGET => Syscall::Shmget,
        i386::SHMCTL => Syscall::Shmctl,
        i386::SHMAT => Syscall::Shmat,
        i386::SHMDT => Syscall::Shmdt,
        i386::PIDFD_OPEN => Syscall::PidfdOpen,
        i386::OPENAT2 => Syscall::Openat2,
        i386::FACCESSAT2 => Syscall::Faccessat2,
        i386::EPOLL_PWAIT2 => Syscall::EpollPwait2,
        i386::PPOLL => Syscall::Ppoll,
        i386::PSELECT6 => Syscall::Pselect6,
        i386::UTIMENSAT => Syscall::Utimensat,
        i386::TIMERFD_SETTIME => Syscall::TimerfdSettime,
        i386::TIMERFD_GETTIME => Syscall::TimerfdGettime,
        i386::CLOCK_ADJTIME => Syscall::ClockAdjtime,
        i386::CLOCK_ADJTIME64 => Syscall::ClockAdjtime64,
        i386::SOCKET => Syscall::Socket,
        i386::SOCKETPAIR => Syscall::Socketpair,
        i386::BIND => Syscall::Bind,
        i386::CONNECT => Syscall::Connect,
        i386::LISTEN => Syscall::Listen,
        i386::ACCEPT4 => Syscall::Accept4,
        i386::GETSOCKOPT => Syscall::Getsockopt,
        i386::SETSOCKOPT => Syscall::Setsockopt,
        i386::GETSOCKNAME => Syscall::Getsockname,
        i386::GETPEERNAME => Syscall::Getpeername,
        i386::SENDTO => Syscall::Sendto,
        i386::SENDMSG => Syscall::Sendmsg,
        i386::RECVFROM => Syscall::Recvfrom,
        i386::RECVMSG => Syscall::Recvmsg,
        i386::SHUTDOWN => Syscall::Shutdown,
        i386::SET_ROBUST_LIST => Syscall::SetRobustList,
        i386::GET_ROBUST_LIST => Syscall::GetRobustList,
        i386::FSTATAT64 => Syscall::Fstatat64,
        i386::RSEQ => Syscall::Rseq,
        _ => return None,
    };
    Some(call)
}
