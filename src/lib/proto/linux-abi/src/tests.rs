//! Layout and mapping assertions.
//!
//! These tests are the reason the ABI lives in a host-buildable crate. A wrong
//! structure size or field offset is not a compile error anywhere in the
//! kernel; it is a program that reads a file size out of padding, and the
//! symptom appears arbitrarily far from the cause. Every number asserted below
//! is the one Linux uses on that architecture.

extern crate std;

use core::mem::{align_of, offset_of, size_of};
use std::collections::HashSet;
use std::vec::Vec;

use crate::errno::{Errno, encode};
use crate::nr::{
    AARCH64_END, ARM_END, ARM_PRIVATE_END, I386_END, Syscall, X86_64_END, aarch64, arm,
    from_aarch64, from_arm, from_i386, from_x86_64, i386, x86_64,
};
use crate::types;

/// The bounds the kernel clamps a number to are the tables' own: nothing at or
/// past one translates, and the number just below it does, so a bound left
/// behind by a table that grew fails here rather than refusing a real call.
#[test]
fn each_table_ends_exactly_at_its_bound() {
    type Translate = fn(usize) -> Option<Syscall>;
    let tables: [(Translate, usize); 4] = [
        (from_x86_64, X86_64_END),
        (from_aarch64, AARCH64_END),
        (from_arm, ARM_END),
        (from_i386, I386_END),
    ];
    for (translate, end) in tables {
        assert!(translate(end - 1).is_some(), "the last number below {end}");
        for number in end..end + 4096 {
            assert!(translate(number).is_none(), "{number} is past {end}");
        }
    }
    for number in ARM_END..arm::ARM_PRIVATE_BASE {
        assert!(
            from_arm(number).is_none(),
            "{number} is between the ARM tables"
        );
    }
    assert!(from_arm(ARM_PRIVATE_END - 1).is_some());
    for number in ARM_PRIVATE_END..ARM_PRIVATE_END + 0x1_0000 {
        assert!(
            from_arm(number).is_none(),
            "{number} is past the ARM-private calls"
        );
    }
}

/// Calls whose number this crate knows on x86-64 but which the generic table
/// never had, because musl reaches the same effect through an `*at` form or a
/// register write.
///
/// Kept as data so `x86_64_only_calls_are_the_expected_ones` can assert the
/// difference between the two tables exactly, rather than spot-checking it.
const X86_64_ONLY: &[Syscall] = &[
    Syscall::Open,
    Syscall::Creat,
    Syscall::Stat,
    Syscall::Lstat,
    Syscall::Poll,
    Syscall::Pipe,
    Syscall::Dup2,
    Syscall::Fork,
    Syscall::Vfork,
    Syscall::Access,
    Syscall::Rename,
    Syscall::Mkdir,
    Syscall::Rmdir,
    Syscall::Unlink,
    Syscall::Symlink,
    Syscall::Link,
    Syscall::Readlink,
    Syscall::Chmod,
    Syscall::Chown,
    Syscall::Lchown,
    Syscall::Mknod,
    Syscall::ArchPrctl,
    Syscall::EpollCreate,
    Syscall::InotifyInit,
    Syscall::Eventfd,
    Syscall::Signalfd,
    Syscall::EpollWait,
    Syscall::Getpgrp,
    Syscall::Alarm,
    Syscall::Time,
    Syscall::Select,
    Syscall::Pause,
];

/// Calls both tables have, with the number each architecture gives them.
///
/// This is a second, independent transcription of the numbers in `nr.rs`: a
/// typo in either copy shows up here as a failure rather than as a call
/// dispatched to the wrong handler.
const SHARED: &[(usize, usize, Syscall)] = &[
    (
        x86_64::RESTART_SYSCALL,
        aarch64::RESTART_SYSCALL,
        Syscall::RestartSyscall,
    ),
    (x86_64::READ, aarch64::READ, Syscall::Read),
    (x86_64::WRITE, aarch64::WRITE, Syscall::Write),
    (x86_64::CLOSE, aarch64::CLOSE, Syscall::Close),
    (x86_64::FSTAT, aarch64::FSTAT, Syscall::Fstat),
    (x86_64::NEWFSTATAT, aarch64::NEWFSTATAT, Syscall::Newfstatat),
    (x86_64::STATX, aarch64::STATX, Syscall::Statx),
    (x86_64::OPENAT, aarch64::OPENAT, Syscall::Openat),
    (x86_64::OPENAT2, aarch64::OPENAT2, Syscall::Openat2),
    (x86_64::SPLICE, aarch64::SPLICE, Syscall::Splice),
    (
        x86_64::COPY_FILE_RANGE,
        aarch64::COPY_FILE_RANGE,
        Syscall::CopyFileRange,
    ),
    (x86_64::LSEEK, aarch64::LSEEK, Syscall::Lseek),
    (x86_64::MMAP, aarch64::MMAP, Syscall::Mmap),
    (x86_64::MPROTECT, aarch64::MPROTECT, Syscall::Mprotect),
    (x86_64::MUNMAP, aarch64::MUNMAP, Syscall::Munmap),
    (x86_64::BRK, aarch64::BRK, Syscall::Brk),
    (
        x86_64::RT_SIGACTION,
        aarch64::RT_SIGACTION,
        Syscall::RtSigaction,
    ),
    (
        x86_64::RT_SIGRETURN,
        aarch64::RT_SIGRETURN,
        Syscall::RtSigreturn,
    ),
    (
        x86_64::SIGALTSTACK,
        aarch64::SIGALTSTACK,
        Syscall::Sigaltstack,
    ),
    (x86_64::IOCTL, aarch64::IOCTL, Syscall::Ioctl),
    (x86_64::WRITEV, aarch64::WRITEV, Syscall::Writev),
    (x86_64::PPOLL, aarch64::PPOLL, Syscall::Ppoll),
    (x86_64::CLONE, aarch64::CLONE, Syscall::Clone),
    (x86_64::CLONE3, aarch64::CLONE3, Syscall::Clone3),
    (x86_64::EXECVE, aarch64::EXECVE, Syscall::Execve),
    (x86_64::EXIT, aarch64::EXIT, Syscall::Exit),
    (x86_64::EXIT_GROUP, aarch64::EXIT_GROUP, Syscall::ExitGroup),
    (x86_64::WAIT4, aarch64::WAIT4, Syscall::Wait4),
    (x86_64::FUTEX, aarch64::FUTEX, Syscall::Futex),
    (
        x86_64::SET_TID_ADDRESS,
        aarch64::SET_TID_ADDRESS,
        Syscall::SetTidAddress,
    ),
    (
        x86_64::SET_ROBUST_LIST,
        aarch64::SET_ROBUST_LIST,
        Syscall::SetRobustList,
    ),
    (
        x86_64::CLOCK_GETTIME,
        aarch64::CLOCK_GETTIME,
        Syscall::ClockGettime,
    ),
    (x86_64::GETDENTS64, aarch64::GETDENTS64, Syscall::Getdents64),
    (x86_64::MKNODAT, aarch64::MKNODAT, Syscall::Mknodat),
    (x86_64::UTIMENSAT, aarch64::UTIMENSAT, Syscall::Utimensat),
    (x86_64::GETRANDOM, aarch64::GETRANDOM, Syscall::Getrandom),
    (x86_64::PRLIMIT64, aarch64::PRLIMIT64, Syscall::Prlimit64),
    (x86_64::UNAME, aarch64::UNAME, Syscall::Uname),
    (x86_64::FACCESSAT2, aarch64::FACCESSAT2, Syscall::Faccessat2),
    (
        x86_64::EPOLL_PWAIT2,
        aarch64::EPOLL_PWAIT2,
        Syscall::EpollPwait2,
    ),
    (
        x86_64::EPOLL_PWAIT,
        aarch64::EPOLL_PWAIT,
        Syscall::EpollPwait,
    ),
    (x86_64::RSEQ, aarch64::RSEQ, Syscall::Rseq),
    (x86_64::MEMBARRIER, aarch64::MEMBARRIER, Syscall::Membarrier),
    (
        x86_64::INOTIFY_INIT1,
        aarch64::INOTIFY_INIT1,
        Syscall::InotifyInit1,
    ),
    (
        x86_64::INOTIFY_ADD_WATCH,
        aarch64::INOTIFY_ADD_WATCH,
        Syscall::InotifyAddWatch,
    ),
    (
        x86_64::INOTIFY_RM_WATCH,
        aarch64::INOTIFY_RM_WATCH,
        Syscall::InotifyRmWatch,
    ),
    (x86_64::PIDFD_OPEN, aarch64::PIDFD_OPEN, Syscall::PidfdOpen),
    (
        x86_64::PIDFD_SEND_SIGNAL,
        aarch64::PIDFD_SEND_SIGNAL,
        Syscall::PidfdSendSignal,
    ),
    (x86_64::SOCKETPAIR, aarch64::SOCKETPAIR, Syscall::Socketpair),
    (x86_64::BIND, aarch64::BIND, Syscall::Bind),
    (x86_64::LISTEN, aarch64::LISTEN, Syscall::Listen),
    (x86_64::ACCEPT, aarch64::ACCEPT, Syscall::Accept),
    (x86_64::ACCEPT4, aarch64::ACCEPT4, Syscall::Accept4),
    (
        x86_64::GETSOCKNAME,
        aarch64::GETSOCKNAME,
        Syscall::Getsockname,
    ),
    (
        x86_64::GETPEERNAME,
        aarch64::GETPEERNAME,
        Syscall::Getpeername,
    ),
    (x86_64::SENDTO, aarch64::SENDTO, Syscall::Sendto),
    (x86_64::RECVFROM, aarch64::RECVFROM, Syscall::Recvfrom),
    (x86_64::SENDMSG, aarch64::SENDMSG, Syscall::Sendmsg),
    (x86_64::RECVMSG, aarch64::RECVMSG, Syscall::Recvmsg),
    (x86_64::SHUTDOWN, aarch64::SHUTDOWN, Syscall::Shutdown),
    (x86_64::SETSOCKOPT, aarch64::SETSOCKOPT, Syscall::Setsockopt),
    (x86_64::GETSOCKOPT, aarch64::GETSOCKOPT, Syscall::Getsockopt),
    (x86_64::SHMGET, aarch64::SHMGET, Syscall::Shmget),
    (x86_64::SHMAT, aarch64::SHMAT, Syscall::Shmat),
    (x86_64::SHMDT, aarch64::SHMDT, Syscall::Shmdt),
    (x86_64::SHMCTL, aarch64::SHMCTL, Syscall::Shmctl),
    (x86_64::MSGGET, aarch64::MSGGET, Syscall::Msgget),
    (x86_64::MSGSND, aarch64::MSGSND, Syscall::Msgsnd),
    (x86_64::MSGRCV, aarch64::MSGRCV, Syscall::Msgrcv),
    (x86_64::MSGCTL, aarch64::MSGCTL, Syscall::Msgctl),
    (x86_64::SEMGET, aarch64::SEMGET, Syscall::Semget),
    (x86_64::SEMOP, aarch64::SEMOP, Syscall::Semop),
    (x86_64::SEMCTL, aarch64::SEMCTL, Syscall::Semctl),
    (x86_64::SEMTIMEDOP, aarch64::SEMTIMEDOP, Syscall::Semtimedop),
    (x86_64::FLOCK, aarch64::FLOCK, Syscall::Flock),
    (x86_64::READAHEAD, aarch64::READAHEAD, Syscall::Readahead),
    (x86_64::SETGROUPS, aarch64::SETGROUPS, Syscall::Setgroups),
    (x86_64::SETREUID, aarch64::SETREUID, Syscall::Setreuid),
    (x86_64::SETREGID, aarch64::SETREGID, Syscall::Setregid),
    (x86_64::SETRESUID, aarch64::SETRESUID, Syscall::Setresuid),
    (x86_64::SETRESGID, aarch64::SETRESGID, Syscall::Setresgid),
    (x86_64::SETFSUID, aarch64::SETFSUID, Syscall::Setfsuid),
    (x86_64::SETFSGID, aarch64::SETFSGID, Syscall::Setfsgid),
    (x86_64::CAPGET, aarch64::CAPGET, Syscall::Capget),
    (x86_64::CAPSET, aarch64::CAPSET, Syscall::Capset),
    (x86_64::SETSID, aarch64::SETSID, Syscall::Setsid),
    (x86_64::GETSID, aarch64::GETSID, Syscall::Getsid),
    (
        x86_64::SETHOSTNAME,
        aarch64::SETHOSTNAME,
        Syscall::Sethostname,
    ),
    (
        x86_64::SETDOMAINNAME,
        aarch64::SETDOMAINNAME,
        Syscall::Setdomainname,
    ),
    (x86_64::SYSLOG, aarch64::SYSLOG, Syscall::Syslog),
    (x86_64::REBOOT, aarch64::REBOOT, Syscall::Reboot),
    (
        x86_64::PERSONALITY,
        aarch64::PERSONALITY,
        Syscall::Personality,
    ),
    (
        x86_64::INIT_MODULE,
        aarch64::INIT_MODULE,
        Syscall::InitModule,
    ),
    (
        x86_64::FINIT_MODULE,
        aarch64::FINIT_MODULE,
        Syscall::FinitModule,
    ),
    (
        x86_64::DELETE_MODULE,
        aarch64::DELETE_MODULE,
        Syscall::DeleteModule,
    ),
    (x86_64::SWAPON, aarch64::SWAPON, Syscall::Swapon),
    (x86_64::SWAPOFF, aarch64::SWAPOFF, Syscall::Swapoff),
    (x86_64::SETNS, aarch64::SETNS, Syscall::Setns),
    (x86_64::VHANGUP, aarch64::VHANGUP, Syscall::Vhangup),
    (x86_64::ACCT, aarch64::ACCT, Syscall::Acct),
    (
        x86_64::SETTIMEOFDAY,
        aarch64::SETTIMEOFDAY,
        Syscall::Settimeofday,
    ),
    (
        x86_64::CLOCK_SETTIME,
        aarch64::CLOCK_SETTIME,
        Syscall::ClockSettime,
    ),
    (x86_64::ADJTIMEX, aarch64::ADJTIMEX, Syscall::Adjtimex),
    (
        x86_64::CLOCK_ADJTIME,
        aarch64::CLOCK_ADJTIME,
        Syscall::ClockAdjtime,
    ),
    (x86_64::TIMES, aarch64::TIMES, Syscall::Times),
    (x86_64::GETITIMER, aarch64::GETITIMER, Syscall::Getitimer),
    (
        x86_64::CLOCK_GETRES,
        aarch64::CLOCK_GETRES,
        Syscall::ClockGetres,
    ),
    (x86_64::SETITIMER, aarch64::SETITIMER, Syscall::Setitimer),
    (
        x86_64::TIMERFD_CREATE,
        aarch64::TIMERFD_CREATE,
        Syscall::TimerfdCreate,
    ),
    (
        x86_64::TIMERFD_SETTIME,
        aarch64::TIMERFD_SETTIME,
        Syscall::TimerfdSettime,
    ),
    (
        x86_64::TIMERFD_GETTIME,
        aarch64::TIMERFD_GETTIME,
        Syscall::TimerfdGettime,
    ),
    (x86_64::SIGNALFD4, aarch64::SIGNALFD4, Syscall::Signalfd4),
    (x86_64::PSELECT6, aarch64::PSELECT6, Syscall::Pselect6),
    (
        x86_64::RT_SIGPENDING,
        aarch64::RT_SIGPENDING,
        Syscall::RtSigpending,
    ),
    (
        x86_64::RT_SIGTIMEDWAIT,
        aarch64::RT_SIGTIMEDWAIT,
        Syscall::RtSigtimedwait,
    ),
    (
        x86_64::RT_SIGQUEUEINFO,
        aarch64::RT_SIGQUEUEINFO,
        Syscall::RtSigqueueinfo,
    ),
    (
        x86_64::SCHED_SETPARAM,
        aarch64::SCHED_SETPARAM,
        Syscall::SchedSetparam,
    ),
    (
        x86_64::SCHED_GET_PRIORITY_MAX,
        aarch64::SCHED_GET_PRIORITY_MAX,
        Syscall::SchedGetPriorityMax,
    ),
    (
        x86_64::SCHED_GET_PRIORITY_MIN,
        aarch64::SCHED_GET_PRIORITY_MIN,
        Syscall::SchedGetPriorityMin,
    ),
    (
        x86_64::SCHED_RR_GET_INTERVAL,
        aarch64::SCHED_RR_GET_INTERVAL,
        Syscall::SchedRrGetInterval,
    ),
    (
        x86_64::GETPRIORITY,
        aarch64::GETPRIORITY,
        Syscall::Getpriority,
    ),
    (
        x86_64::SETPRIORITY,
        aarch64::SETPRIORITY,
        Syscall::Setpriority,
    ),
    (x86_64::IOPRIO_GET, aarch64::IOPRIO_GET, Syscall::IoprioGet),
    (x86_64::IOPRIO_SET, aarch64::IOPRIO_SET, Syscall::IoprioSet),
    (x86_64::GETCPU, aarch64::GETCPU, Syscall::Getcpu),
];

/// Every call either table maps, found by sweeping the number space.
fn mapped(translate: fn(usize) -> Option<Syscall>) -> HashSet<Syscall> {
    (0..=600).filter_map(translate).collect()
}

/// The numbers an ARMv7-A program can arrive with.
///
/// Not a single range: the EABI table runs from zero, and the six ARM-private
/// calls sit at `0x0f0000`. A sweep that stopped at the shared table's top
/// would silently exclude `set_tls`, which is the one call a threaded program
/// cannot start without.
fn arm_number_space() -> impl Iterator<Item = usize> {
    (0..=600).chain(arm::ARM_PRIVATE_BASE..=arm::ARM_PRIVATE_BASE + 16)
}

/// Every call the ARMv7-A table maps, private range included.
fn mapped_arm() -> HashSet<Syscall> {
    arm_number_space().filter_map(from_arm).collect()
}

// ---------------------------------------------------------------------------
// Time, vectors and directories
// ---------------------------------------------------------------------------

#[test]
fn timespec_and_timeval_are_two_64_bit_words() {
    assert_eq!(
        size_of::<types::Timespec>(),
        16,
        "timespec is two 64-bit fields"
    );
    assert_eq!(
        size_of::<types::Timeval>(),
        16,
        "timeval is two 64-bit fields"
    );
    assert_eq!(
        offset_of!(types::Timespec, tv_nsec),
        8,
        "tv_nsec follows tv_sec"
    );
    assert_eq!(
        offset_of!(types::Timeval, tv_usec),
        8,
        "tv_usec follows tv_sec"
    );
}

#[test]
fn iovec_is_a_pointer_and_a_length() {
    assert_eq!(
        size_of::<types::Iovec>(),
        16,
        "iovec is a base and a length"
    );
    assert_eq!(
        offset_of!(types::Iovec, iov_len),
        8,
        "iov_len follows iov_base"
    );
}

#[test]
fn dirent64_header_matches_the_kernel() {
    assert_eq!(
        offset_of!(types::LinuxDirent64, d_ino),
        0,
        "d_ino comes first"
    );
    assert_eq!(
        offset_of!(types::LinuxDirent64, d_off),
        8,
        "d_off follows d_ino"
    );
    assert_eq!(
        offset_of!(types::LinuxDirent64, d_reclen),
        16,
        "d_reclen follows d_off"
    );
    assert_eq!(
        offset_of!(types::LinuxDirent64, d_type),
        18,
        "d_type follows d_reclen"
    );
    assert_eq!(
        offset_of!(types::LinuxDirent64, d_name),
        types::DIRENT64_NAME_OFFSET,
        "the name starts immediately after d_type, before any tail padding"
    );
}

#[test]
fn dirent64_size_is_not_the_name_offset() {
    assert_eq!(
        size_of::<types::LinuxDirent64>(),
        24,
        "the head rounds up to 8-byte alignment"
    );
    assert!(
        types::DIRENT64_NAME_OFFSET < size_of::<types::LinuxDirent64>(),
        "using size_of instead of the name offset would skip the first bytes of every name"
    );
}

// ---------------------------------------------------------------------------
// stat, per architecture
// ---------------------------------------------------------------------------

#[test]
fn x86_64_stat_is_144_bytes() {
    assert_eq!(
        size_of::<types::x86_64::Stat>(),
        144,
        "x86-64 struct stat is 144 bytes"
    );
    assert_eq!(
        align_of::<types::x86_64::Stat>(),
        8,
        "every field is 8-byte aligned or smaller"
    );
}

#[test]
fn x86_64_stat_field_offsets() {
    type S = types::x86_64::Stat;
    assert_eq!(offset_of!(S, st_dev), 0, "st_dev comes first");
    assert_eq!(offset_of!(S, st_ino), 8, "st_ino follows st_dev");
    assert_eq!(
        offset_of!(S, st_nlink),
        16,
        "x86-64 puts st_nlink before st_mode"
    );
    assert_eq!(
        offset_of!(S, st_mode),
        24,
        "st_mode is a 32-bit field at 24"
    );
    assert_eq!(offset_of!(S, st_uid), 28, "st_uid follows st_mode");
    assert_eq!(offset_of!(S, st_gid), 32, "st_gid follows st_uid");
    assert_eq!(
        offset_of!(S, st_rdev),
        40,
        "st_rdev follows the explicit pad"
    );
    assert_eq!(
        offset_of!(S, st_size),
        48,
        "a wrong st_size offset reads garbage file sizes"
    );
    assert_eq!(
        offset_of!(S, st_blksize),
        56,
        "st_blksize is 64-bit on x86-64"
    );
    assert_eq!(offset_of!(S, st_blocks), 64, "st_blocks follows st_blksize");
    assert_eq!(offset_of!(S, st_atime), 72, "the timestamps start at 72");
    assert_eq!(
        offset_of!(S, st_mtime),
        88,
        "st_mtime follows st_atime and its nanoseconds"
    );
    assert_eq!(
        offset_of!(S, st_ctime),
        104,
        "st_ctime follows st_mtime and its nanoseconds"
    );
    assert_eq!(
        offset_of!(S, __unused),
        120,
        "three reserved words close the structure"
    );
}

#[test]
fn aarch64_stat_is_128_bytes() {
    assert_eq!(
        size_of::<types::aarch64::Stat>(),
        128,
        "the generic struct stat is 128 bytes"
    );
    assert_eq!(
        align_of::<types::aarch64::Stat>(),
        8,
        "every field is 8-byte aligned or smaller"
    );
}

// ---------------------------------------------------------------------------
// statfs, per word size
//
// Every offset below is read off `include/uapi/asm-generic/statfs.h` with
// `__statfs_word` substituted, and for `statfs64` with
// `arch/arm/include/uapi/asm/statfs.h`'s `packed,aligned(4)` applied.
// ---------------------------------------------------------------------------

#[test]
fn statfs_on_64_bit_is_120_bytes_with_every_count_a_word() {
    type S = types::Statfs;
    assert_eq!(size_of::<S>(), 120, "the 64-bit struct statfs is 120 bytes");
    assert_eq!(offset_of!(S, f_type), 0, "f_type comes first");
    assert_eq!(offset_of!(S, f_bsize), 8, "f_bsize is the second word");
    assert_eq!(offset_of!(S, f_blocks), 16, "f_blocks follows f_bsize");
    assert_eq!(offset_of!(S, f_bfree), 24, "f_bfree follows f_blocks");
    assert_eq!(offset_of!(S, f_bavail), 32, "f_bavail follows f_bfree");
    assert_eq!(offset_of!(S, f_files), 40, "f_files follows f_bavail");
    assert_eq!(offset_of!(S, f_ffree), 48, "f_ffree follows f_files");
    assert_eq!(offset_of!(S, f_fsid), 56, "the fsid is two ints at 56");
    assert_eq!(offset_of!(S, f_namelen), 64, "f_namelen follows the fsid");
    assert_eq!(offset_of!(S, f_frsize), 72, "f_frsize follows f_namelen");
    assert_eq!(offset_of!(S, f_flags), 80, "f_flags follows f_frsize");
    assert_eq!(offset_of!(S, f_spare), 88, "four spare words close it");
}

#[test]
fn statfs_on_armv7a_is_64_bytes_of_32_bit_words() {
    type S = types::ArmStatfs;
    assert_eq!(size_of::<S>(), 64, "ARMv7-A's struct statfs is 64 bytes");
    assert_eq!(offset_of!(S, f_bsize), 4, "f_bsize is the second word");
    assert_eq!(offset_of!(S, f_ffree), 24, "f_ffree is the seventh word");
    assert_eq!(offset_of!(S, f_fsid), 28, "the fsid follows f_ffree");
    assert_eq!(offset_of!(S, f_namelen), 36, "f_namelen follows the fsid");
    assert_eq!(offset_of!(S, f_frsize), 40, "f_frsize follows f_namelen");
    assert_eq!(offset_of!(S, f_flags), 44, "f_flags follows f_frsize");
    assert_eq!(offset_of!(S, f_spare), 48, "four spare words close it");
}

#[test]
fn statfs64_on_armv7a_is_packed_to_84_bytes() {
    type S = types::ArmStatfs64;
    assert_eq!(
        size_of::<S>(),
        84,
        "packed,aligned(4) removes the four bytes the EABI would add at the end"
    );
    assert_eq!(align_of::<S>(), 4, "aligned(4)");
    assert_eq!(offset_of!(S, f_bsize), 4, "f_bsize is 32 bits");
    assert_eq!(offset_of!(S, f_blocks), 8, "the 64-bit counts start at 8");
    assert_eq!(offset_of!(S, f_bfree), 16, "f_bfree follows f_blocks");
    assert_eq!(offset_of!(S, f_bavail), 24, "f_bavail follows f_bfree");
    assert_eq!(offset_of!(S, f_files), 32, "f_files follows f_bavail");
    assert_eq!(offset_of!(S, f_ffree), 40, "f_ffree follows f_files");
    assert_eq!(offset_of!(S, f_fsid), 48, "the fsid follows the counts");
    assert_eq!(offset_of!(S, f_namelen), 56, "f_namelen follows the fsid");
    assert_eq!(offset_of!(S, f_frsize), 60, "f_frsize follows f_namelen");
    assert_eq!(offset_of!(S, f_flags), 64, "f_flags follows f_frsize");
    assert_eq!(offset_of!(S, f_spare), 68, "four spare words close it");
    assert_eq!(
        types::ARM_STATFS64_UNPACKED_SIZE,
        size_of::<S>() + 4,
        "musl's unpacked structure is the packed one plus the EABI's tail padding"
    );
}

#[test]
fn aarch64_stat_field_offsets() {
    type S = types::aarch64::Stat;
    assert_eq!(offset_of!(S, st_dev), 0, "st_dev comes first");
    assert_eq!(offset_of!(S, st_ino), 8, "st_ino follows st_dev");
    assert_eq!(
        offset_of!(S, st_mode),
        16,
        "the generic layout puts st_mode before st_nlink"
    );
    assert_eq!(
        offset_of!(S, st_nlink),
        20,
        "st_nlink is 32-bit here, 64-bit on x86-64"
    );
    assert_eq!(offset_of!(S, st_uid), 24, "st_uid follows st_nlink");
    assert_eq!(offset_of!(S, st_gid), 28, "st_gid follows st_uid");
    assert_eq!(offset_of!(S, st_rdev), 32, "st_rdev follows st_gid");
    assert_eq!(
        offset_of!(S, st_size),
        48,
        "a wrong st_size offset reads garbage file sizes"
    );
    assert_eq!(
        offset_of!(S, st_blksize),
        56,
        "st_blksize is only 32 bits wide here"
    );
    assert_eq!(
        offset_of!(S, st_blocks),
        64,
        "st_blocks follows the pad after st_blksize"
    );
    assert_eq!(
        offset_of!(S, st_atime),
        72,
        "the timestamps start at 72, as on x86-64"
    );
    assert_eq!(
        offset_of!(S, st_ctime),
        104,
        "st_ctime follows st_mtime and its nanoseconds"
    );
    assert_eq!(
        offset_of!(S, __unused),
        120,
        "two reserved words close the structure"
    );
}

#[test]
fn the_two_stat_layouts_really_do_differ() {
    assert_ne!(
        size_of::<types::x86_64::Stat>(),
        size_of::<types::aarch64::Stat>(),
        "using one architecture's stat on the other is the bug these types exist to prevent"
    );
    assert_ne!(
        offset_of!(types::x86_64::Stat, st_mode),
        offset_of!(types::aarch64::Stat, st_mode),
        "st_mode moves between the two layouts"
    );
}

// ---------------------------------------------------------------------------
// statx
// ---------------------------------------------------------------------------

#[test]
fn arm_stat64_is_104_bytes() {
    assert_eq!(
        size_of::<types::arm::Stat64>(),
        104,
        "ARM EABI struct stat64 is 104 bytes"
    );
    assert_eq!(
        align_of::<types::arm::Stat64>(),
        8,
        "the EABI aligns a 64-bit field to eight bytes"
    );
}

#[test]
fn arm_stat64_field_offsets() {
    // Against `arch/arm/include/uapi/asm/stat.h` with the EABI's alignment,
    // and against QEMU's packed `target_eabi_stat64`, which spells out the
    // same padding.
    type S = types::arm::Stat64;
    assert_eq!(offset_of!(S, st_dev), 0, "st_dev comes first");
    assert_eq!(offset_of!(S, __st_ino), 12, "the truncated inode at 12");
    assert_eq!(offset_of!(S, st_mode), 16, "st_mode follows __st_ino");
    assert_eq!(offset_of!(S, st_nlink), 20, "st_nlink follows st_mode");
    assert_eq!(offset_of!(S, st_uid), 24, "st_uid is a 32-bit long");
    assert_eq!(offset_of!(S, st_gid), 28, "st_gid follows st_uid");
    assert_eq!(offset_of!(S, st_rdev), 32, "st_rdev is aligned to 32");
    assert_eq!(
        offset_of!(S, st_size),
        48,
        "st_size after eight bytes of padding, four of them implicit in C"
    );
    assert_eq!(offset_of!(S, st_blksize), 56, "st_blksize is 32-bit");
    assert_eq!(
        offset_of!(S, st_blocks),
        64,
        "st_blocks is aligned past the implicit pad"
    );
    assert_eq!(offset_of!(S, st_atime), 72, "32-bit times begin at 72");
    assert_eq!(offset_of!(S, st_mtime), 80, "st_mtime follows atime");
    assert_eq!(offset_of!(S, st_ctime), 88, "st_ctime follows mtime");
    assert_eq!(offset_of!(S, st_ctime_nsec), 92, "the last time field");
    assert_eq!(offset_of!(S, st_ino), 96, "the real inode number is last");
}

#[test]
fn path_call_flags_match_the_uapi_headers() {
    // `include/uapi/linux/fcntl.h`, `include/uapi/linux/fs.h`,
    // `include/linux/stat.h` and `include/uapi/linux/stat.h`.
    assert_eq!(types::AT_NO_AUTOMOUNT, 0x800, "AT_NO_AUTOMOUNT");
    assert_eq!(types::AT_STATX_SYNC_TYPE, 0x6000, "AT_STATX_SYNC_TYPE");
    assert_eq!(
        types::AT_EACCESS,
        types::AT_REMOVEDIR,
        "AT_EACCESS shares AT_REMOVEDIR's bit"
    );
    assert_eq!(
        (
            types::RENAME_NOREPLACE,
            types::RENAME_EXCHANGE,
            types::RENAME_WHITEOUT
        ),
        (1, 2, 4),
        "RENAME_* flags"
    );
    assert_eq!(types::UTIME_NOW, 0x3fff_ffff, "UTIME_NOW is (1 << 30) - 1");
    assert_eq!(
        types::UTIME_OMIT,
        0x3fff_fffe,
        "UTIME_OMIT is (1 << 30) - 2"
    );
    assert_eq!(
        (types::F_OK, types::X_OK, types::W_OK, types::R_OK),
        (0, 1, 2, 4),
        "access modes"
    );
    assert_eq!(types::STATX_MNT_ID, 0x1000, "STATX_MNT_ID");
    assert_eq!(types::STATX_RESERVED, 0x8000_0000, "STATX__RESERVED");
}

#[test]
fn statx_timestamp_is_16_bytes() {
    assert_eq!(
        size_of::<types::StatxTimestamp>(),
        16,
        "a statx timestamp is 16 bytes"
    );
    assert_eq!(
        offset_of!(types::StatxTimestamp, tv_nsec),
        8,
        "tv_nsec follows tv_sec"
    );
}

#[test]
fn statx_is_256_bytes() {
    assert_eq!(
        size_of::<types::Statx>(),
        256,
        "struct statx is 256 bytes, reserved space included"
    );
    assert_eq!(
        align_of::<types::Statx>(),
        8,
        "struct statx is 8-byte aligned"
    );
}

#[test]
fn statx_field_offsets() {
    type S = types::Statx;
    assert_eq!(offset_of!(S, stx_mask), 0, "the mask comes first");
    assert_eq!(
        offset_of!(S, stx_blksize),
        4,
        "stx_blksize follows the mask"
    );
    assert_eq!(
        offset_of!(S, stx_attributes),
        8,
        "stx_attributes is 8-byte aligned"
    );
    assert_eq!(
        offset_of!(S, stx_mode),
        28,
        "stx_mode is a 16-bit field at 28"
    );
    assert_eq!(
        offset_of!(S, stx_ino),
        32,
        "stx_ino follows the spare half-word"
    );
    assert_eq!(offset_of!(S, stx_size), 40, "stx_size follows stx_ino");
    assert_eq!(
        offset_of!(S, stx_atime),
        64,
        "the four timestamps start at 64"
    );
    assert_eq!(offset_of!(S, stx_btime), 80, "stx_btime follows stx_atime");
    assert_eq!(offset_of!(S, stx_ctime), 96, "stx_ctime follows stx_btime");
    assert_eq!(
        offset_of!(S, stx_mtime),
        112,
        "stx_mtime is last of the four"
    );
    assert_eq!(
        offset_of!(S, stx_rdev_major),
        128,
        "the device numbers follow the timestamps"
    );
    assert_eq!(
        offset_of!(S, stx_mnt_id),
        144,
        "stx_mnt_id follows the four 32-bit device numbers at 128..144"
    );
}

// ---------------------------------------------------------------------------
// Identity, limits and usage
// ---------------------------------------------------------------------------

#[test]
fn utsname_is_six_65_byte_strings() {
    assert_eq!(
        size_of::<types::Utsname>(),
        390,
        "six fields of __NEW_UTS_LEN + 1 bytes"
    );
    assert_eq!(
        align_of::<types::Utsname>(),
        1,
        "an array of bytes needs no alignment"
    );
    assert_eq!(
        offset_of!(types::Utsname, machine),
        260,
        "machine is the fifth field"
    );
    assert_eq!(
        offset_of!(types::Utsname, domainname),
        325,
        "domainname is the sixth field"
    );
}

#[test]
fn rlimit_is_two_words() {
    assert_eq!(
        size_of::<types::Rlimit>(),
        16,
        "rlimit is a soft and a hard limit"
    );
    assert_eq!(
        offset_of!(types::Rlimit, rlim_max),
        8,
        "the hard limit follows the soft one"
    );
}

#[test]
fn rusage_is_144_bytes() {
    assert_eq!(
        size_of::<types::Rusage>(),
        144,
        "two timevals and fourteen longs"
    );
    assert_eq!(
        offset_of!(types::Rusage, ru_stime),
        16,
        "system time follows user time"
    );
    assert_eq!(
        offset_of!(types::Rusage, ru_maxrss),
        32,
        "the counters start after the times"
    );
    assert_eq!(
        offset_of!(types::Rusage, ru_minflt),
        64,
        "minor faults are the fifth counter"
    );
    assert_eq!(
        offset_of!(types::Rusage, ru_nivcsw),
        136,
        "involuntary switches close it"
    );
}

#[test]
fn sysinfo_is_112_bytes_on_64_bit() {
    assert_eq!(
        size_of::<types::Sysinfo>(),
        112,
        "the 64-bit form leaves no room for _f"
    );
    assert_eq!(
        offset_of!(types::Sysinfo, loads),
        8,
        "the load averages follow uptime"
    );
    assert_eq!(
        offset_of!(types::Sysinfo, totalram),
        32,
        "memory counts follow the loads"
    );
    assert_eq!(
        offset_of!(types::Sysinfo, procs),
        80,
        "procs is a 16-bit field at 80"
    );
    assert_eq!(
        offset_of!(types::Sysinfo, totalhigh),
        88,
        "alignment pads before totalhigh"
    );
    assert_eq!(
        offset_of!(types::Sysinfo, mem_unit),
        104,
        "mem_unit is the last real field"
    );
}

// ---------------------------------------------------------------------------
// Signals and polling
// ---------------------------------------------------------------------------

#[test]
fn sigset_is_one_word() {
    assert_eq!(
        size_of::<types::Sigset>(),
        8,
        "the kernel's sigset_t is 8 bytes; the C library's is not, which is why \
         rt_sigprocmask takes the size as an argument"
    );
    assert_eq!(
        size_of::<types::Sigset>() * 8,
        types::NSIG as usize,
        "one bit per signal"
    );
}

#[test]
fn sigaction_has_the_kernel_field_order() {
    type S = types::Sigaction;
    assert_eq!(
        size_of::<S>(),
        32,
        "handler, flags, restorer and an 8-byte mask"
    );
    assert_eq!(offset_of!(S, sa_handler), 0, "the handler comes first");
    assert_eq!(
        offset_of!(S, sa_flags),
        8,
        "flags follow the handler, unlike in POSIX"
    );
    assert_eq!(
        offset_of!(S, sa_restorer),
        16,
        "the restorer sits before the mask"
    );
    assert_eq!(
        offset_of!(S, sa_mask),
        24,
        "the mask is last so the structure can grow"
    );
}

#[test]
fn stack_t_is_24_bytes() {
    assert_eq!(
        size_of::<types::Stack>(),
        24,
        "a pointer, an int and a size with padding"
    );
    assert_eq!(
        offset_of!(types::Stack, ss_flags),
        8,
        "ss_flags follows ss_sp"
    );
    assert_eq!(
        offset_of!(types::Stack, ss_size),
        16,
        "ss_size is realigned to 16"
    );
}

#[test]
fn pollfd_is_8_bytes() {
    assert_eq!(size_of::<types::Pollfd>(), 8, "an int and two shorts");
    assert_eq!(offset_of!(types::Pollfd, events), 4, "events follows fd");
    assert_eq!(
        offset_of!(types::Pollfd, revents),
        6,
        "revents follows events"
    );
}

#[test]
fn epoll_event_is_packed_on_x86_64_only() {
    assert_eq!(
        size_of::<types::x86_64::EpollEvent>(),
        12,
        "x86-64 defines EPOLL_PACKED so that 32-bit processes see the same layout"
    );
    assert_eq!(
        offset_of!(types::x86_64::EpollEvent, data),
        4,
        "packed: data follows events"
    );
    assert_eq!(
        size_of::<types::aarch64::EpollEvent>(),
        16,
        "AArch64 uses the natural layout"
    );
    assert_eq!(
        offset_of!(types::aarch64::EpollEvent, data),
        8,
        "unpacked: data is aligned to 8"
    );
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

#[test]
fn the_architecture_dependent_open_flags_match_their_headers() {
    // A second transcription, from `asm-generic/fcntl.h` and from the arm and
    // arm64 `asm/fcntl.h`, in the headers' own octal.
    let generic = types::OPEN_FLAGS_GENERIC;
    assert_eq!(generic.direct, 0o40000, "generic O_DIRECT");
    assert_eq!(generic.largefile, 0o100000, "generic O_LARGEFILE");
    assert_eq!(generic.directory, 0o200000, "generic O_DIRECTORY");
    assert_eq!(generic.nofollow, 0o400000, "generic O_NOFOLLOW");
    let arm = types::OPEN_FLAGS_ARM;
    assert_eq!(arm.directory, 0o40000, "arm O_DIRECTORY");
    assert_eq!(arm.nofollow, 0o100000, "arm O_NOFOLLOW");
    assert_eq!(arm.direct, 0o200000, "arm O_DIRECT");
    assert_eq!(arm.largefile, 0o400000, "arm O_LARGEFILE");
    assert_eq!(
        generic.directory,
        types::O_DIRECTORY,
        "the shared constant is generic"
    );
    assert_eq!(
        generic.nofollow,
        types::O_NOFOLLOW,
        "the shared constant is generic"
    );
}

#[test]
fn both_open_flag_tables_are_four_distinct_bits_clear_of_the_shared_flags() {
    let shared = types::O_ACCMODE
        | types::O_CREAT
        | types::O_EXCL
        | types::O_NOCTTY
        | types::O_TRUNC
        | types::O_APPEND
        | types::O_NONBLOCK
        | types::O_CLOEXEC
        | types::O_PATH;
    let bits = |t: types::OpenFlagBits| [t.direct, t.largefile, t.directory, t.nofollow];
    for table in [types::OPEN_FLAGS_GENERIC, types::OPEN_FLAGS_ARM] {
        let all = bits(table);
        assert!(
            all.iter().all(|bit| bit.count_ones() == 1),
            "each flag is a single bit"
        );
        let union = all.iter().fold(0, |acc, bit| acc | bit);
        assert_eq!(union.count_ones(), 4, "the four flags are distinct");
        assert_eq!(union & shared, 0, "no flag collides with a shared one");
    }
    let union = |t| bits(t).iter().fold(0, |acc, bit| acc | bit);
    assert_eq!(
        union(types::OPEN_FLAGS_GENERIC),
        union(types::OPEN_FLAGS_ARM),
        "Arm permutes the generic bits rather than using new ones"
    );
    assert_ne!(
        types::OPEN_FLAGS_GENERIC,
        types::OPEN_FLAGS_ARM,
        "and the permutation is not the identity"
    );
}

#[test]
fn terminal_ioctls_match_the_generic_header() {
    assert_eq!(types::TCGETS, 0x5401, "TCGETS is 'T' 0x01");
    assert_eq!(types::TIOCGWINSZ, 0x5413, "TIOCGWINSZ is 'T' 0x13");
    assert_eq!(types::TCSETSF, 0x5404, "TCSETSF is 'T' 0x04");
    assert_eq!(types::TCFLSH, 0x540B, "TCFLSH is 'T' 0x0B");
    assert_eq!(types::TIOCSPGRP, 0x5410, "TIOCSPGRP is 'T' 0x10");
    assert_eq!(types::FIONREAD, 0x541B, "FIONREAD is 'T' 0x1B");
    assert_eq!(types::FIONBIO, 0x5421, "FIONBIO is 'T' 0x21");
    assert_eq!(types::FIONCLEX, 0x5450, "FIONCLEX is 'T' 0x50");
    assert_eq!(types::FIOCLEX, 0x5451, "FIOCLEX is 'T' 0x51");
    assert_eq!(types::TIOCINQ, types::FIONREAD, "TIOCINQ is FIONREAD");
    assert_eq!(types::TIOCNOTTY, 0x5422, "TIOCNOTTY is 'T' 0x22");
    assert_eq!(types::TIOCGSID, 0x5429, "TIOCGSID is 'T' 0x29");

    // The two pseudoterminal requests carry a direction and a size, which
    // the plain `'T' nn` numbers above do not: `_IOR` is two in the top two
    // bits, `_IOW` is one, and the size is the argument's.
    let encode = |direction: u32, size: u32, number: u32| {
        (direction << 30) | (size << 16) | (u32::from(b'T') << 8) | number
    };
    assert_eq!(
        types::TIOCGPTN,
        encode(2, 4, 0x30),
        "TIOCGPTN is _IOR('T', 0x30, unsigned int)"
    );
    assert_eq!(
        types::TIOCSPTLCK,
        encode(1, 4, 0x31),
        "TIOCSPTLCK is _IOW('T', 0x31, int)"
    );
}

#[test]
fn termios2_requests_carry_their_structure_size() {
    // `_IOC(dir, type, nr, size)` in `asm-generic/ioctl.h`: eight bits of
    // number, eight of type, fourteen of size, two of direction, with
    // `_IOC_READ` 2 and `_IOC_WRITE` 1. x86-64, AArch64 and ARMv7-A all take
    // the generic encoding.
    let size = u32::try_from(types::TERMIOS2_BYTES).expect("a small size");
    let ioc = |direction: u32, nr: u32| (direction << 30) | (size << 16) | (0x54 << 8) | nr;
    assert_eq!(types::TCGETS2, ioc(2, 0x2A), "TCGETS2 is _IOR('T', 0x2A)");
    assert_eq!(types::TCSETS2, ioc(1, 0x2B), "TCSETS2 is _IOW('T', 0x2B)");
    assert_eq!(types::TCSETSW2, ioc(1, 0x2C), "TCSETSW2 is _IOW('T', 0x2C)");
    assert_eq!(types::TCSETSF2, ioc(1, 0x2D), "TCSETSF2 is _IOW('T', 0x2D)");
    // What the disassembly of glibc's `tcgetattr` passes.
    assert_eq!(types::TCGETS2, 0x802C_542A);
}

#[test]
fn termios2_matches_the_generic_termbits_header() {
    #[repr(C)]
    struct Termios2 {
        c_iflag: u32,
        c_oflag: u32,
        c_cflag: u32,
        c_lflag: u32,
        c_line: u8,
        c_cc: [u8; types::NCCS],
        c_ispeed: u32,
        c_ospeed: u32,
    }
    assert_eq!(size_of::<Termios2>(), types::TERMIOS2_BYTES);
    assert_eq!(offset_of!(Termios2, c_cc), 17);
    assert_eq!(offset_of!(Termios2, c_ispeed), types::TERMIOS_BYTES);
    assert_eq!(offset_of!(Termios2, c_ospeed), types::TERMIOS_BYTES + 4);
    assert_eq!(align_of::<u32>(), 4, "speed_t aligns to four on all three");
}

#[test]
fn baud_rates_match_the_termbits_headers() {
    assert_eq!(
        (types::CBAUD, types::BOTHER, types::IBSHIFT),
        (0x100F, 0x1000, 16)
    );
    let codes: HashSet<u32> = types::BAUD_RATES.iter().map(|&(code, _)| code).collect();
    assert_eq!(
        codes.len(),
        types::BAUD_RATES.len(),
        "every code is listed once"
    );
    assert!(!codes.contains(&types::BOTHER), "BOTHER names no rate");
    assert!(codes.iter().all(|code| code & !types::CBAUD == 0));
    assert!(
        types::BAUD_RATES
            .windows(2)
            .all(|pair| pair[0].1 < pair[1].1),
        "the rates rise with the codes"
    );
    for (code, rate) in [
        (0x000D, 9600),
        (0x000F, 38400),
        (0x1001, 57600),
        (0x1002, 115_200),
    ] {
        assert!(
            types::BAUD_RATES.contains(&(code, rate)),
            "{code:#x} is {rate}"
        );
    }
    assert!(types::BAUD_RATES.contains(&(0x100F, 4_000_000)), "B4000000");
}

#[test]
fn termios_matches_the_generic_termbits_header() {
    // Four `tcflag_t`, `c_line`, `c_cc[NCCS]`: no padding, since every field
    // after the flags is a byte.
    assert_eq!(types::TERMIOS_BYTES, 4 * 4 + 1 + types::NCCS);
    assert_eq!(types::NCCS, 19, "NCCS is 19 in asm-generic/termbits.h");
    assert_eq!((types::VMIN, types::VTIME, types::VEOL2), (6, 5, 16));
    assert_eq!((types::ICANON, types::ECHO, types::IEXTEN), (2, 8, 0x8000));
    assert_eq!((types::ICRNL, types::OPOST, types::ONLCR), (0x100, 1, 4));
}

#[test]
fn flock_operations_match_the_generic_header() {
    // `asm-generic/fcntl.h`, which all three architectures use unchanged.
    assert_eq!(
        (
            types::LOCK_SH,
            types::LOCK_EX,
            types::LOCK_NB,
            types::LOCK_UN
        ),
        (1, 2, 4, 8)
    );
}

#[test]
fn record_lock_commands_match_the_generic_header() {
    // `asm-generic/fcntl.h`. The `64` commands are ARMv7-A's alone: Linux's
    // `fcntl64` exists only where a long is 32 bits.
    assert_eq!((types::F_GETLK, types::F_SETLK, types::F_SETLKW), (5, 6, 7));
    assert_eq!(
        (types::F_GETLK64, types::F_SETLK64, types::F_SETLKW64),
        (12, 13, 14)
    );
    assert_eq!(
        (types::F_OFD_GETLK, types::F_OFD_SETLK, types::F_OFD_SETLKW),
        (36, 37, 38)
    );
    assert_eq!((types::F_RDLCK, types::F_WRLCK, types::F_UNLCK), (0, 1, 2));
}

#[test]
fn open_flags_match_the_generic_header() {
    assert_eq!(types::O_CREAT, 64, "O_CREAT is octal 100");
    assert_eq!(types::O_TRUNC, 512, "O_TRUNC is octal 1000");
    assert_eq!(types::O_APPEND, 1024, "O_APPEND is octal 2000");
    assert_eq!(types::O_NONBLOCK, 2048, "O_NONBLOCK is octal 4000");
    assert_eq!(types::O_DIRECTORY, 65536, "O_DIRECTORY is octal 200000");
    assert_eq!(types::O_CLOEXEC, 524_288, "O_CLOEXEC is octal 2000000");
    assert_eq!(
        types::EFD_CLOEXEC,
        types::O_CLOEXEC,
        "eventfd reuses the open flag"
    );
}

#[test]
fn timerfd_flags_match_the_uapi_header() {
    // `include/uapi/linux/timerfd.h`: the two creation flags are the open
    // flags, and the two setting flags are bits 0 and 1, which only
    // `timerfd_settime` reads.
    assert_eq!(types::TFD_CLOEXEC, 0o2_000_000, "TFD_CLOEXEC is O_CLOEXEC");
    assert_eq!(types::TFD_NONBLOCK, 0o4000, "TFD_NONBLOCK is O_NONBLOCK");
    assert_eq!(types::TFD_TIMER_ABSTIME, 1, "TFD_TIMER_ABSTIME is bit 0");
    assert_eq!(
        types::TFD_TIMER_CANCEL_ON_SET,
        2,
        "TFD_TIMER_CANCEL_ON_SET is bit 1"
    );
}

#[test]
fn signalfd_flags_match_the_uapi_header() {
    // `include/uapi/linux/signalfd.h`: the two flags are the open flags, and
    // `struct signalfd_siginfo` is padded to 128 bytes.
    assert_eq!(types::SFD_CLOEXEC, 0o2_000_000, "SFD_CLOEXEC is O_CLOEXEC");
    assert_eq!(types::SFD_NONBLOCK, 0o4000, "SFD_NONBLOCK is O_NONBLOCK");
    assert_eq!(
        types::SIGNALFD_SIGINFO_BYTES,
        128,
        "signalfd_siginfo is 128 bytes"
    );
}

#[test]
fn madvise_advice_matches_the_generic_header() {
    // `include/uapi/asm-generic/mman-common.h`, which none of the three
    // architectures overrides. 5 to 7 are unused.
    let advice = [
        (types::MADV_NORMAL, 0),
        (types::MADV_RANDOM, 1),
        (types::MADV_SEQUENTIAL, 2),
        (types::MADV_WILLNEED, 3),
        (types::MADV_DONTNEED, 4),
        (types::MADV_FREE, 8),
        (types::MADV_REMOVE, 9),
        (types::MADV_DONTFORK, 10),
        (types::MADV_DOFORK, 11),
        (types::MADV_MERGEABLE, 12),
        (types::MADV_UNMERGEABLE, 13),
        (types::MADV_HUGEPAGE, 14),
        (types::MADV_NOHUGEPAGE, 15),
        (types::MADV_DONTDUMP, 16),
        (types::MADV_DODUMP, 17),
        (types::MADV_WIPEONFORK, 18),
        (types::MADV_KEEPONFORK, 19),
        (types::MADV_COLD, 20),
        (types::MADV_PAGEOUT, 21),
        (types::MADV_POPULATE_READ, 22),
        (types::MADV_POPULATE_WRITE, 23),
        (types::MADV_DONTNEED_LOCKED, 24),
        (types::MADV_COLLAPSE, 25),
    ];
    for (constant, header) in advice {
        assert_eq!(constant, header, "an MADV_ value differs from the header");
    }
}

#[test]
fn file_type_bits_are_disjoint_under_the_mask() {
    let types_seen = [
        types::S_IFIFO,
        types::S_IFCHR,
        types::S_IFDIR,
        types::S_IFBLK,
        types::S_IFREG,
        types::S_IFLNK,
        types::S_IFSOCK,
    ];
    let unique: HashSet<u32> = types_seen.iter().copied().collect();
    assert_eq!(
        unique.len(),
        types_seen.len(),
        "every file type has a distinct encoding"
    );
    assert!(
        types_seen.iter().all(|&t| t & types::S_IFMT == t),
        "every file type fits inside S_IFMT"
    );
}

#[test]
fn at_fdcwd_is_negative_and_not_minus_one() {
    assert_eq!(types::AT_FDCWD, -100, "AT_FDCWD is -100");
    assert_ne!(
        types::AT_FDCWD,
        -1,
        "a stray -1 must not be mistaken for the working directory"
    );
}

#[test]
fn auxv_keys_musl_startup_reads() {
    assert_eq!(types::AT_PHDR, 3, "the program headers key");
    assert_eq!(types::AT_PAGESZ, 6, "the page size key");
    assert_eq!(types::AT_ENTRY, 9, "the entry point key");
    assert_eq!(
        types::AT_SECURE,
        23,
        "the key musl checks before trusting the environment"
    );
    assert_eq!(
        types::AT_RANDOM,
        25,
        "the key musl seeds its stack guard from"
    );
    assert_eq!(types::AT_SYSINFO_EHDR, 33, "the vDSO key");
    assert_eq!(types::AT_MINSIGSTKSZ, 51, "the minimum signal stack key");
}

#[test]
fn signal_numbers_are_the_generic_ones() {
    assert_eq!(types::SIGKILL, 9, "SIGKILL is 9");
    assert_eq!(types::SIGSEGV, 11, "SIGSEGV is 11");
    assert_eq!(
        types::SIGCHLD,
        17,
        "SIGCHLD is 17 on Linux, unlike on other UNIX systems"
    );
    assert_eq!(types::SIGSTOP, 19, "SIGSTOP is 19");
    assert_eq!(types::SIGSYS, 31, "SIGSYS closes the classic range");
    assert_eq!(
        types::SIGRTMIN,
        32,
        "the kernel's first real-time signal is 32"
    );
}

#[test]
fn clock_identifiers_are_the_generic_ones() {
    assert_eq!(types::CLOCK_REALTIME, 0, "the wall clock is 0");
    assert_eq!(types::CLOCK_MONOTONIC, 1, "the monotonic clock is 1");
    assert_eq!(
        types::CLOCK_MONOTONIC_RAW,
        4,
        "the unadjusted monotonic clock is 4"
    );
    assert_eq!(types::CLOCK_BOOTTIME, 7, "the suspend-aware clock is 7");
}

// ---------------------------------------------------------------------------
// errno
// ---------------------------------------------------------------------------

#[test]
fn errno_values_are_the_generic_ones() {
    assert_eq!(Errno::EPERM.0, 1, "EPERM is 1");
    assert_eq!(Errno::ENOENT.0, 2, "ENOENT is 2");
    assert_eq!(
        Errno::EAGAIN.0,
        11,
        "EAGAIN, which is also EWOULDBLOCK, is 11"
    );
    assert_eq!(Errno::EINVAL.0, 22, "EINVAL is 22");
    assert_eq!(Errno::ERANGE.0, 34, "ERANGE closes the errno-base range");
    assert_eq!(
        Errno::ENOSYS.0,
        38,
        "ENOSYS is what an unknown call number returns"
    );
    assert_eq!(Errno::ENODATA.0, 61, "ENODATA is what a missing xattr is");
    assert_eq!(Errno::EOVERFLOW.0, 75, "EOVERFLOW is 75");
    assert_eq!(Errno::EBADFD.0, 77, "EBADFD is 77, asm-generic/errno.h");
    assert_eq!(Errno::ESTRPIPE.0, 86, "ESTRPIPE is 86, asm-generic/errno.h");
    assert_eq!(Errno::ECONNREFUSED.0, 111, "ECONNREFUSED is 111");
    assert_eq!(Errno::ECANCELED.0, 125, "ECANCELED is 125");
}

#[test]
fn errno_encodes_as_a_negative_return_value() {
    assert_eq!(Errno::EPERM.as_return_value(), -1, "EPERM returns as -1");
    assert_eq!(
        Errno::ENOSYS.as_return_value(),
        -38,
        "ENOSYS returns as -38"
    );
    assert!(
        Errno::EINPROGRESS.as_return_value() > -(crate::errno::MAX_ERRNO as isize),
        "every error stays inside the range reserved for them"
    );
}

#[test]
fn encode_maps_results_onto_the_return_register() {
    assert_eq!(encode(Ok(0)), 0, "a successful zero is returned unchanged");
    assert_eq!(
        encode(Ok(4096)),
        4096,
        "a successful count is returned unchanged"
    );
    assert_eq!(encode(Err(Errno::ENOENT)), -2, "ENOENT is returned as -2");
    assert_eq!(encode(Err(Errno::EPERM)), -1, "EPERM is returned as -1");
}

// ---------------------------------------------------------------------------
// Syscall number translation
// ---------------------------------------------------------------------------

#[test]
fn x86_64_round_trips() {
    assert_eq!(
        from_x86_64(x86_64::WRITE),
        Some(Syscall::Write),
        "write is 1 on x86-64"
    );
    assert_eq!(
        from_x86_64(x86_64::READ),
        Some(Syscall::Read),
        "read is 0 on x86-64"
    );
    assert_eq!(
        from_x86_64(x86_64::OPEN),
        Some(Syscall::Open),
        "open is 2 on x86-64"
    );
    assert_eq!(
        from_x86_64(x86_64::MMAP),
        Some(Syscall::Mmap),
        "mmap is 9 on x86-64"
    );
    assert_eq!(
        from_x86_64(x86_64::CLONE3),
        Some(Syscall::Clone3),
        "clone3 is 435"
    );
}

#[test]
fn aarch64_round_trips() {
    assert_eq!(
        from_aarch64(aarch64::WRITE),
        Some(Syscall::Write),
        "write is 64 on AArch64"
    );
    assert_eq!(
        from_aarch64(aarch64::READ),
        Some(Syscall::Read),
        "read is 63 on AArch64"
    );
    assert_eq!(
        from_aarch64(aarch64::OPENAT),
        Some(Syscall::Openat),
        "openat is 56 on AArch64"
    );
    assert_eq!(
        from_aarch64(aarch64::MMAP),
        Some(Syscall::Mmap),
        "mmap is 222 on AArch64"
    );
    assert_eq!(
        from_aarch64(aarch64::CLONE3),
        Some(Syscall::Clone3),
        "clone3 is 435 on both"
    );
}

#[test]
fn shared_calls_agree_across_architectures() {
    for &(x86, arm, call) in SHARED {
        assert_eq!(
            from_x86_64(x86),
            Some(call),
            "x86-64 number {x86} should be {call:?}"
        );
        assert_eq!(
            from_aarch64(arm),
            Some(call),
            "AArch64 number {arm} should be {call:?}"
        );
    }
}

#[test]
fn shared_table_mostly_uses_different_numbers() {
    let differing = SHARED.iter().filter(|&&(x86, arm, _)| x86 != arm).count();
    assert!(
        differing > SHARED.len() / 2,
        "the two tables genuinely disagree, so a single dispatch table would be wrong"
    );
}

#[test]
fn aarch64_has_no_path_only_calls() {
    for &(nr, name) in &[
        (x86_64::OPEN, "open"),
        (x86_64::FORK, "fork"),
        (x86_64::DUP2, "dup2"),
        (x86_64::STAT, "stat"),
        (x86_64::LSTAT, "lstat"),
        (x86_64::POLL, "poll"),
        (x86_64::PIPE, "pipe"),
        (x86_64::ACCESS, "access"),
        (x86_64::ARCH_PRCTL, "arch_prctl"),
    ] {
        let mapped_to = from_aarch64(nr);
        assert!(
            mapped_to != Some(Syscall::Open) && mapped_to != Some(Syscall::Fork),
            "AArch64 must not resolve {name}'s x86-64 number to the same call"
        );
    }
    assert_eq!(
        from_aarch64(2),
        None,
        "generic syscall 2 is io_destroy, which this crate does not carry -- and it \n         is certainly not `open`, which the generic table has no number for at all"
    );
}

/// `time` is 201 on x86-64 (`asm/unistd_64.h`) and absent from the other two
/// tables: the generic one never had it, and ARM's is in `unistd-oabi.h` only,
/// so EABI number 13 is not it.
#[test]
fn time_has_an_x86_64_number_only() {
    assert_eq!(x86_64::TIME, 201, "time is 201 on x86-64");
    assert_eq!(from_x86_64(201), Some(Syscall::Time));
    assert!(!mapped(from_aarch64).contains(&Syscall::Time));
    assert!(!mapped_arm().contains(&Syscall::Time));
    assert_eq!(from_arm(13), None, "EABI has no call at OABI's time number");
}

#[test]
fn x86_64_only_calls_are_the_expected_ones() {
    let only_on_x86: HashSet<Syscall> = mapped(from_x86_64)
        .difference(&mapped(from_aarch64))
        .copied()
        .collect();
    let expected: HashSet<Syscall> = X86_64_ONLY.iter().copied().collect();
    let mut missing: Vec<&Syscall> = expected.difference(&only_on_x86).collect();
    missing.sort_unstable();
    let mut extra: Vec<&Syscall> = only_on_x86.difference(&expected).collect();
    extra.sort_unstable();
    assert!(
        missing.is_empty(),
        "expected to be x86-64 only but AArch64 maps them: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "x86-64 only, but not listed as such: {extra:?}"
    );
}

#[test]
fn aarch64_maps_no_call_x86_64_lacks() {
    let only_on_arm: HashSet<Syscall> = mapped(from_aarch64)
        .difference(&mapped(from_x86_64))
        .copied()
        .collect();
    assert!(
        only_on_arm.is_empty(),
        "every AArch64 call also has an x86-64 number: {only_on_arm:?}"
    );
}

#[test]
fn no_two_numbers_map_to_the_same_call() {
    let mut seen = HashSet::new();
    for nr in 0..=600 {
        if let Some(call) = from_x86_64(nr) {
            assert!(seen.insert(call), "two x86-64 numbers both map to {call:?}");
        }
    }
    let mut seen = HashSet::new();
    for nr in 0..=600 {
        if let Some(call) = from_aarch64(nr) {
            assert!(
                seen.insert(call),
                "two AArch64 numbers both map to {call:?}"
            );
        }
    }
}

#[test]
fn unknown_numbers_map_to_none_without_panicking() {
    for nr in [
        78,
        174,
        101,
        132,
        134,
        136,
        139,
        149,
        300,
        400,
        436,
        438,
        440,
        500,
        1000,
        4096,
        usize::MAX / 2,
        usize::MAX - 1,
        usize::MAX,
    ] {
        assert_eq!(
            from_x86_64(nr),
            None,
            "x86-64 number {nr} is not one this crate knows"
        );
    }
    for nr in [
        0,
        1,
        2,
        3,
        4,
        18,
        42,
        60,
        70,
        75,
        77,
        109,
        110,
        250,
        300,
        400,
        436,
        438,
        440,
        1000,
        usize::MAX / 2,
        usize::MAX - 1,
        usize::MAX,
    ] {
        assert_eq!(
            from_aarch64(nr),
            None,
            "AArch64 number {nr} is not one this crate knows"
        );
    }
}

#[test]
fn the_whole_number_space_is_total() {
    for nr in 0..=1200 {
        let _x86 = from_x86_64(nr);
        let _aarch64 = from_aarch64(nr);
        let _arm = from_arm(nr);
        let _i386 = from_i386(nr);
    }
    assert_eq!(
        from_i386(usize::MAX),
        None,
        "the largest number is answered, not trapped"
    );
    assert_eq!(
        from_x86_64(usize::MAX),
        None,
        "the largest number is answered, not trapped"
    );
    assert_eq!(
        from_aarch64(usize::MAX),
        None,
        "the largest number is answered, not trapped"
    );
    assert_eq!(
        from_arm(usize::MAX),
        None,
        "the largest number is answered, not trapped"
    );
}

#[test]
fn both_tables_cover_the_calls_musl_startup_makes() {
    let x86 = mapped(from_x86_64);
    let arm = mapped(from_aarch64);
    for call in [
        Syscall::Brk,
        Syscall::Mmap,
        Syscall::Mprotect,
        Syscall::SetTidAddress,
        Syscall::SetRobustList,
        Syscall::RtSigprocmask,
        Syscall::Readlinkat,
        Syscall::Writev,
        Syscall::ExitGroup,
        Syscall::Futex,
        Syscall::ClockGettime,
        Syscall::Getrandom,
    ] {
        assert!(x86.contains(&call), "x86-64 must dispatch {call:?}");
        assert!(arm.contains(&call), "AArch64 must dispatch {call:?}");
    }
}

#[test]
fn table_sizes_are_stable() {
    // A canary, not a specification. The numbers are whatever the tables
    // currently hold; the point is that adding or losing a call is a visible
    // change to this line rather than something nobody notices. It caught
    // `socket` being unreachable on AArch64.
    assert_eq!(
        mapped(from_x86_64).len(),
        254,
        "the x86-64 table maps 254 calls"
    );
    assert_eq!(
        mapped(from_aarch64).len(),
        222,
        "the AArch64 table maps 222 calls"
    );
}
/// Calls only ARMv7-A has, because it is the only 32-bit target.
///
/// Every one of them exists because a 32-bit register cannot carry what the
/// call has to pass, or because ARM userspace cannot reach a register the
/// other two write for themselves. None is a synonym: each takes arguments
/// its 64-bit namesake does not.
const ARM_ONLY: &[Syscall] = &[
    Syscall::Stat64,
    Syscall::Lstat64,
    Syscall::Fstat64,
    Syscall::Fstatat64,
    Syscall::Statfs64,
    Syscall::Fstatfs64,
    Syscall::Truncate64,
    Syscall::Ftruncate64,
    Syscall::Sendfile64,
    Syscall::Fcntl64,
    Syscall::Llseek,
    Syscall::Mmap2,
    Syscall::ClockGettime64,
    Syscall::ClockNanosleepTime64,
    Syscall::UtimensatTime64,
    Syscall::PpollTime64,
    Syscall::FutexTime64,
    Syscall::Sigreturn,
    Syscall::ArmSetTls,
    Syscall::ArmCacheflush,
    Syscall::ClockSettime64,
    Syscall::ClockAdjtime64,
    Syscall::Pselect6Time64,
    Syscall::RtSigtimedwaitTime64,
    Syscall::SchedRrGetIntervalTime64,
    Syscall::SemtimedopTime64,
    Syscall::TimerfdSettime64,
    Syscall::TimerfdGettime64,
    Syscall::ClockGetresTime64,
];

/// Every ARMv7-A number this crate knows, paired with the call it means.
///
/// The same role `SHARED` plays for the other two tables, but stronger: these
/// are literal numbers rather than references to `nr::arm`, so a mistyped
/// constant fails this test rather than silently dispatching a program's
/// `write` into, say, `unlink`.
const ARM_NUMBERS: &[(usize, Syscall)] = &[
    // Generated against `arch/arm/include/uapi/asm/unistd-common.h`; the
    // number is written out rather than taken from `nr::arm` so that a
    // wrong constant fails here instead of dispatching a call to the
    // wrong handler.
    (0, Syscall::RestartSyscall),             // restart_syscall
    (1, Syscall::Exit),                       // exit
    (2, Syscall::Fork),                       // fork
    (3, Syscall::Read),                       // read
    (4, Syscall::Write),                      // write
    (5, Syscall::Open),                       // open
    (6, Syscall::Close),                      // close
    (8, Syscall::Creat),                      // creat
    (9, Syscall::Link),                       // link
    (10, Syscall::Unlink),                    // unlink
    (11, Syscall::Execve),                    // execve
    (12, Syscall::Chdir),                     // chdir
    (14, Syscall::Mknod),                     // mknod
    (15, Syscall::Chmod),                     // chmod
    (19, Syscall::Lseek),                     // lseek
    (20, Syscall::Getpid),                    // getpid
    (21, Syscall::Mount),                     // mount
    (29, Syscall::Pause),                     // pause
    (33, Syscall::Access),                    // access
    (36, Syscall::Sync),                      // sync
    (37, Syscall::Kill),                      // kill
    (38, Syscall::Rename),                    // rename
    (39, Syscall::Mkdir),                     // mkdir
    (40, Syscall::Rmdir),                     // rmdir
    (41, Syscall::Dup),                       // dup
    (42, Syscall::Pipe),                      // pipe
    (43, Syscall::Times),                     // times
    (45, Syscall::Brk),                       // brk
    (51, Syscall::Acct),                      // acct
    (52, Syscall::Umount2),                   // umount2
    (54, Syscall::Ioctl),                     // ioctl
    (55, Syscall::Fcntl),                     // fcntl
    (57, Syscall::Setpgid),                   // setpgid
    (60, Syscall::Umask),                     // umask
    (63, Syscall::Dup2),                      // dup2
    (64, Syscall::Getppid),                   // getppid
    (65, Syscall::Getpgrp),                   // getpgrp
    (66, Syscall::Setsid),                    // setsid
    (74, Syscall::Sethostname),               // sethostname
    (75, Syscall::Setrlimit),                 // setrlimit
    (77, Syscall::Getrusage),                 // getrusage
    (78, Syscall::Gettimeofday),              // gettimeofday
    (79, Syscall::Settimeofday),              // settimeofday
    (83, Syscall::Symlink),                   // symlink
    (85, Syscall::Readlink),                  // readlink
    (87, Syscall::Swapon),                    // swapon
    (88, Syscall::Reboot),                    // reboot
    (91, Syscall::Munmap),                    // munmap
    (92, Syscall::Truncate),                  // truncate
    (93, Syscall::Ftruncate),                 // ftruncate
    (94, Syscall::Fchmod),                    // fchmod
    (96, Syscall::Getpriority),               // getpriority
    (97, Syscall::Setpriority),               // setpriority
    (103, Syscall::Syslog),                   // syslog
    (104, Syscall::Setitimer),                // setitimer
    (105, Syscall::Getitimer),                // getitimer
    (111, Syscall::Vhangup),                  // vhangup
    (114, Syscall::Wait4),                    // wait4
    (115, Syscall::Swapoff),                  // swapoff
    (116, Syscall::Sysinfo),                  // sysinfo
    (118, Syscall::Fsync),                    // fsync
    (119, Syscall::Sigreturn),                // sigreturn
    (120, Syscall::Clone),                    // clone
    (121, Syscall::Setdomainname),            // setdomainname
    (122, Syscall::Uname),                    // uname
    (124, Syscall::Adjtimex),                 // adjtimex
    (125, Syscall::Mprotect),                 // mprotect
    (128, Syscall::InitModule),               // init_module
    (129, Syscall::DeleteModule),             // delete_module
    (132, Syscall::Getpgid),                  // getpgid
    (133, Syscall::Fchdir),                   // fchdir
    (136, Syscall::Personality),              // personality
    (140, Syscall::Llseek),                   // _llseek
    (142, Syscall::Select),                   // _newselect
    (143, Syscall::Flock),                    // flock
    (144, Syscall::Msync),                    // msync
    (145, Syscall::Readv),                    // readv
    (146, Syscall::Writev),                   // writev
    (147, Syscall::Getsid),                   // getsid
    (148, Syscall::Fdatasync),                // fdatasync
    (154, Syscall::SchedSetparam),            // sched_setparam
    (155, Syscall::SchedGetparam),            // sched_getparam
    (156, Syscall::SchedSetscheduler),        // sched_setscheduler
    (157, Syscall::SchedGetscheduler),        // sched_getscheduler
    (158, Syscall::SchedYield),               // sched_yield
    (159, Syscall::SchedGetPriorityMax),      // sched_get_priority_max
    (160, Syscall::SchedGetPriorityMin),      // sched_get_priority_min
    (161, Syscall::SchedRrGetInterval),       // sched_rr_get_interval
    (162, Syscall::Nanosleep),                // nanosleep
    (163, Syscall::Mremap),                   // mremap
    (168, Syscall::Poll),                     // poll
    (172, Syscall::Prctl),                    // prctl
    (173, Syscall::RtSigreturn),              // rt_sigreturn
    (174, Syscall::RtSigaction),              // rt_sigaction
    (175, Syscall::RtSigprocmask),            // rt_sigprocmask
    (176, Syscall::RtSigpending),             // rt_sigpending
    (177, Syscall::RtSigtimedwait),           // rt_sigtimedwait
    (178, Syscall::RtSigqueueinfo),           // rt_sigqueueinfo
    (179, Syscall::RtSigsuspend),             // rt_sigsuspend
    (180, Syscall::Pread64),                  // pread64
    (181, Syscall::Pwrite64),                 // pwrite64
    (183, Syscall::Getcwd),                   // getcwd
    (184, Syscall::Capget),                   // capget
    (185, Syscall::Capset),                   // capset
    (186, Syscall::Sigaltstack),              // sigaltstack
    (190, Syscall::Vfork),                    // vfork
    (191, Syscall::Getrlimit),                // ugetrlimit
    (192, Syscall::Mmap2),                    // mmap2
    (193, Syscall::Truncate64),               // truncate64
    (194, Syscall::Ftruncate64),              // ftruncate64
    (195, Syscall::Stat64),                   // stat64
    (196, Syscall::Lstat64),                  // lstat64
    (197, Syscall::Fstat64),                  // fstat64
    (198, Syscall::Lchown),                   // lchown32
    (199, Syscall::Getuid),                   // getuid32
    (200, Syscall::Getgid),                   // getgid32
    (201, Syscall::Geteuid),                  // geteuid32
    (202, Syscall::Getegid),                  // getegid32
    (203, Syscall::Setreuid),                 // setreuid32
    (204, Syscall::Setregid),                 // setregid32
    (205, Syscall::Getgroups),                // getgroups32
    (206, Syscall::Setgroups),                // setgroups32
    (207, Syscall::Fchown),                   // fchown32
    (208, Syscall::Setresuid),                // setresuid32
    (209, Syscall::Getresuid),                // getresuid32
    (210, Syscall::Setresgid),                // setresgid32
    (211, Syscall::Getresgid),                // getresgid32
    (212, Syscall::Chown),                    // chown32
    (213, Syscall::Setuid),                   // setuid32
    (214, Syscall::Setgid),                   // setgid32
    (215, Syscall::Setfsuid),                 // setfsuid32
    (216, Syscall::Setfsgid),                 // setfsgid32
    (217, Syscall::Getdents64),               // getdents64
    (220, Syscall::Madvise),                  // madvise
    (221, Syscall::Fcntl64),                  // fcntl64
    (224, Syscall::Gettid),                   // gettid
    (225, Syscall::Readahead),                // readahead
    (238, Syscall::Tkill),                    // tkill
    (239, Syscall::Sendfile64),               // sendfile64
    (240, Syscall::Futex),                    // futex
    (241, Syscall::SchedSetaffinity),         // sched_setaffinity
    (242, Syscall::SchedGetaffinity),         // sched_getaffinity
    (248, Syscall::ExitGroup),                // exit_group
    (250, Syscall::EpollCreate),              // epoll_create
    (251, Syscall::EpollCtl),                 // epoll_ctl
    (252, Syscall::EpollWait),                // epoll_wait
    (256, Syscall::SetTidAddress),            // set_tid_address
    (262, Syscall::ClockSettime),             // clock_settime
    (263, Syscall::ClockGettime),             // clock_gettime
    (264, Syscall::ClockGetres),              // clock_getres
    (265, Syscall::ClockNanosleep),           // clock_nanosleep
    (266, Syscall::Statfs64),                 // statfs64
    (267, Syscall::Fstatfs64),                // fstatfs64
    (268, Syscall::Tgkill),                   // tgkill
    (280, Syscall::Waitid),                   // waitid
    (281, Syscall::Socket),                   // socket
    (282, Syscall::Bind),                     // bind
    (283, Syscall::Connect),                  // connect
    (284, Syscall::Listen),                   // listen
    (285, Syscall::Accept),                   // accept
    (286, Syscall::Getsockname),              // getsockname
    (287, Syscall::Getpeername),              // getpeername
    (288, Syscall::Socketpair),               // socketpair
    (290, Syscall::Sendto),                   // sendto
    (292, Syscall::Recvfrom),                 // recvfrom
    (293, Syscall::Shutdown),                 // shutdown
    (294, Syscall::Setsockopt),               // setsockopt
    (295, Syscall::Getsockopt),               // getsockopt
    (296, Syscall::Sendmsg),                  // sendmsg
    (297, Syscall::Recvmsg),                  // recvmsg
    (298, Syscall::Semop),                    // semop
    (299, Syscall::Semget),                   // semget
    (300, Syscall::Semctl),                   // semctl
    (301, Syscall::Msgsnd),                   // msgsnd
    (302, Syscall::Msgrcv),                   // msgrcv
    (303, Syscall::Msgget),                   // msgget
    (304, Syscall::Msgctl),                   // msgctl
    (305, Syscall::Shmat),                    // shmat
    (306, Syscall::Shmdt),                    // shmdt
    (307, Syscall::Shmget),                   // shmget
    (308, Syscall::Shmctl),                   // shmctl
    (312, Syscall::Semtimedop),               // semtimedop
    (314, Syscall::IoprioSet),                // ioprio_set
    (315, Syscall::IoprioGet),                // ioprio_get
    (316, Syscall::InotifyInit),              // inotify_init
    (317, Syscall::InotifyAddWatch),          // inotify_add_watch
    (318, Syscall::InotifyRmWatch),           // inotify_rm_watch
    (322, Syscall::Openat),                   // openat
    (323, Syscall::Mkdirat),                  // mkdirat
    (324, Syscall::Mknodat),                  // mknodat
    (325, Syscall::Fchownat),                 // fchownat
    (327, Syscall::Fstatat64),                // fstatat64
    (328, Syscall::Unlinkat),                 // unlinkat
    (329, Syscall::Renameat),                 // renameat
    (330, Syscall::Linkat),                   // linkat
    (331, Syscall::Symlinkat),                // symlinkat
    (332, Syscall::Readlinkat),               // readlinkat
    (333, Syscall::Fchmodat),                 // fchmodat
    (334, Syscall::Faccessat),                // faccessat
    (335, Syscall::Pselect6),                 // pselect6
    (336, Syscall::Ppoll),                    // ppoll
    (337, Syscall::Unshare),                  // unshare
    (338, Syscall::SetRobustList),            // set_robust_list
    (339, Syscall::GetRobustList),            // get_robust_list
    (340, Syscall::Splice),                   // splice
    (345, Syscall::Getcpu),                   // getcpu
    (346, Syscall::EpollPwait),               // epoll_pwait
    (348, Syscall::Utimensat),                // utimensat
    (349, Syscall::Signalfd),                 // signalfd
    (350, Syscall::TimerfdCreate),            // timerfd_create
    (351, Syscall::Eventfd),                  // eventfd
    (353, Syscall::TimerfdSettime),           // timerfd_settime
    (354, Syscall::TimerfdGettime),           // timerfd_gettime
    (355, Syscall::Signalfd4),                // signalfd4
    (356, Syscall::Eventfd2),                 // eventfd2
    (357, Syscall::EpollCreate1),             // epoll_create1
    (358, Syscall::Dup3),                     // dup3
    (359, Syscall::Pipe2),                    // pipe2
    (360, Syscall::InotifyInit1),             // inotify_init1
    (366, Syscall::Accept4),                  // accept4
    (369, Syscall::Prlimit64),                // prlimit64
    (370, Syscall::NameToHandleAt),           // name_to_handle_at
    (372, Syscall::ClockAdjtime),             // clock_adjtime
    (375, Syscall::Setns),                    // setns
    (379, Syscall::FinitModule),              // finit_module
    (382, Syscall::Renameat2),                // renameat2
    (384, Syscall::Getrandom),                // getrandom
    (385, Syscall::MemfdCreate),              // memfd_create
    (387, Syscall::Execveat),                 // execveat
    (389, Syscall::Membarrier),               // membarrier
    (391, Syscall::CopyFileRange),            // copy_file_range
    (397, Syscall::Statx),                    // statx
    (398, Syscall::Rseq),                     // rseq
    (403, Syscall::ClockGettime64),           // clock_gettime64
    (404, Syscall::ClockSettime64),           // clock_settime64
    (405, Syscall::ClockAdjtime64),           // clock_adjtime64
    (406, Syscall::ClockGetresTime64),        // clock_getres_time64
    (407, Syscall::ClockNanosleepTime64),     // clock_nanosleep_time64
    (410, Syscall::TimerfdGettime64),         // timerfd_gettime64
    (411, Syscall::TimerfdSettime64),         // timerfd_settime64
    (412, Syscall::UtimensatTime64),          // utimensat_time64
    (413, Syscall::Pselect6Time64),           // pselect6_time64
    (414, Syscall::PpollTime64),              // ppoll_time64
    (420, Syscall::SemtimedopTime64),         // semtimedop_time64
    (421, Syscall::RtSigtimedwaitTime64),     // rt_sigtimedwait_time64
    (422, Syscall::FutexTime64),              // futex_time64
    (423, Syscall::SchedRrGetIntervalTime64), // sched_rr_get_interval_time64
    (424, Syscall::PidfdSendSignal),          // pidfd_send_signal
    (434, Syscall::PidfdOpen),                // pidfd_open
    (435, Syscall::Clone3),                   // clone3
    (437, Syscall::Openat2),                  // openat2
    (439, Syscall::Faccessat2),               // faccessat2
    (441, Syscall::EpollPwait2),              // epoll_pwait2
];

// ---------------------------------------------------------------------------
// The ARMv7-A (EABI) table
// ---------------------------------------------------------------------------

#[test]
fn arm_numbers_all_resolve_to_the_call_they_name() {
    for &(nr, call) in ARM_NUMBERS {
        assert_eq!(
            from_arm(nr),
            Some(call),
            "ARMv7-A number {nr} must dispatch {call:?}"
        );
    }
}

#[test]
fn arm_round_trips() {
    assert_eq!(
        from_arm(arm::READ),
        Some(Syscall::Read),
        "read is 3 on EABI"
    );
    assert_eq!(
        from_arm(arm::WRITE),
        Some(Syscall::Write),
        "write is 4 on EABI"
    );
    assert_eq!(
        from_arm(arm::EXIT_GROUP),
        Some(Syscall::ExitGroup),
        "exit_group is 248 on EABI"
    );
    assert_eq!(
        from_arm(arm::CLONE3),
        Some(Syscall::Clone3),
        "numbers assigned after the generic table froze match everywhere"
    );
}

#[test]
fn arm_numbers_are_the_eabi_ones_not_the_oabi_ones() {
    // OABI based every number at 0x900000. If a constant had been taken from
    // that table the whole file would be shifted, and `read` is the cheapest
    // place to notice.
    assert_eq!(arm::READ, 3, "EABI's __NR_SYSCALL_BASE is zero");
    assert_eq!(
        from_arm(0x0090_0003),
        None,
        "an OABI-based number is not a number this kernel answers"
    );
}

#[test]
fn arm_uses_the_wide_forms_a_32_bit_musl_actually_calls() {
    // Each of these is the pair that a naive "ARM is x86-64 with different
    // numbers" table would get wrong, and each would be wrong silently.
    assert_eq!(
        from_arm(arm::MMAP2),
        Some(Syscall::Mmap2),
        "mmap2 is a different call from mmap: its offset counts pages"
    );
    assert_eq!(
        from_arm(arm::FSTAT64),
        Some(Syscall::Fstat64),
        "fstat64 writes struct stat64, which is not struct stat"
    );
    assert_eq!(
        from_arm(arm::LLSEEK),
        Some(Syscall::Llseek),
        "_llseek returns its offset through a pointer"
    );
    assert_eq!(
        from_arm(arm::CLOCK_GETTIME64),
        Some(Syscall::ClockGettime64),
        "musl has been time64 since 1.2"
    );
    assert_eq!(
        from_arm(arm::FUTEX_TIME64),
        Some(Syscall::FutexTime64),
        "a time64 musl's locks wait on futex_time64"
    );
}

#[test]
fn arm_16_bit_credential_calls_are_absent() {
    // Deliberate: musl issues only the `32` forms, so an arrival on one of the
    // pre-2.4 numbers is more likely a mistake than a request.
    for (nr, name) in [
        (24, "getuid"),
        (47, "getgid"),
        (49, "geteuid"),
        (23, "setuid"),
        (70, "setreuid"),
        (71, "setregid"),
        (80, "getgroups"),
        (81, "setgroups"),
        (138, "setfsuid"),
        (139, "setfsgid"),
        (164, "setresuid"),
        (165, "getresuid"),
        (170, "setresgid"),
        (171, "getresgid"),
    ] {
        assert_eq!(
            from_arm(nr),
            None,
            "the 16-bit {name} is not carried, and answers ENOSYS"
        );
    }
    assert_eq!(
        from_arm(arm::GETUID32),
        Some(Syscall::Getuid),
        "the 32-bit form is the one that dispatches"
    );
}

#[test]
fn arm_private_calls_sit_far_above_the_shared_table() {
    assert_eq!(
        arm::ARM_PRIVATE_BASE,
        0x000f_0000,
        "__ARM_NR_BASE, with EABI's zero syscall base"
    );
    assert_eq!(from_arm(arm::ARM_SET_TLS), Some(Syscall::ArmSetTls));
    assert_eq!(from_arm(arm::ARM_CACHEFLUSH), Some(Syscall::ArmCacheflush));
    // The gap is the whole point: no number the shared table will ever grow
    // into can collide with one of these.
    let highest_shared = ARM_NUMBERS
        .iter()
        .map(|&(nr, _)| nr)
        .max()
        .expect("the table is not empty");
    assert!(
        highest_shared < arm::ARM_PRIVATE_BASE,
        "the private range must stay above every shared number"
    );
}

#[test]
fn arm_only_calls_are_the_expected_ones() {
    let sixty_four_bit: HashSet<Syscall> = mapped(from_x86_64)
        .union(&mapped(from_aarch64))
        .copied()
        .collect();
    let only_on_arm: HashSet<Syscall> = mapped_arm().difference(&sixty_four_bit).copied().collect();
    let expected: HashSet<Syscall> = ARM_ONLY.iter().copied().collect();
    let mut missing: Vec<&Syscall> = expected.difference(&only_on_arm).collect();
    missing.sort_unstable();
    let mut extra: Vec<&Syscall> = only_on_arm.difference(&expected).collect();
    extra.sort_unstable();
    assert!(
        missing.is_empty(),
        "expected to be ARMv7-A only, but a 64-bit table maps them: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "ARMv7-A only, but not listed as such: {extra:?}"
    );
}

#[test]
fn arm_no_two_numbers_map_to_the_same_call() {
    let mut seen = HashSet::new();
    for nr in arm_number_space() {
        if let Some(call) = from_arm(nr) {
            assert!(
                seen.insert(call),
                "two ARMv7-A numbers both map to {call:?}"
            );
        }
    }
}

#[test]
fn arm_covers_the_calls_musl_startup_makes() {
    // The 32-bit spellings of `both_tables_cover_the_calls_musl_startup_makes`.
    // Three entries differ from the 64-bit list, and those three are exactly
    // the reason this test is written out separately rather than folded in.
    let arm_calls = mapped_arm();
    for call in [
        Syscall::Brk,
        Syscall::Mmap2,
        Syscall::Mprotect,
        Syscall::SetTidAddress,
        Syscall::SetRobustList,
        Syscall::RtSigprocmask,
        Syscall::Readlinkat,
        Syscall::Writev,
        Syscall::ExitGroup,
        Syscall::FutexTime64,
        Syscall::ClockGettime64,
        Syscall::Getrandom,
        Syscall::ArmSetTls,
    ] {
        assert!(arm_calls.contains(&call), "ARMv7-A must dispatch {call:?}");
    }
}

#[test]
fn arm_table_size_is_stable() {
    // A canary, as for the other two tables.
    assert_eq!(mapped_arm().len(), 272, "the ARMv7-A table maps 272 calls");
}

/// The filesystem-control and extended-attribute calls, against the numbers in
/// `arch/x86/entry/syscalls/syscall_64.tbl`, `include/uapi/asm-generic/unistd.h`
/// and `arch/arm/include/uapi/asm/unistd-common.h`, read from the kernel's
/// own headers rather than recalled.
#[test]
fn filesystem_control_calls_match_the_kernel_tables() {
    for (x86, generic, eabi, call) in [
        (188, 5, 226, Syscall::Setxattr),
        (189, 6, 227, Syscall::Lsetxattr),
        (190, 7, 228, Syscall::Fsetxattr),
        (191, 8, 229, Syscall::Getxattr),
        (192, 9, 230, Syscall::Lgetxattr),
        (193, 10, 231, Syscall::Fgetxattr),
        (194, 11, 232, Syscall::Listxattr),
        (195, 12, 233, Syscall::Llistxattr),
        (196, 13, 234, Syscall::Flistxattr),
        (197, 14, 235, Syscall::Removexattr),
        (198, 15, 236, Syscall::Lremovexattr),
        (199, 16, 237, Syscall::Fremovexattr),
        (155, 41, 218, Syscall::PivotRoot),
        (285, 47, 352, Syscall::Fallocate),
        (161, 51, 61, Syscall::Chroot),
        (306, 267, 373, Syscall::Syncfs),
        (303, 264, 370, Syscall::NameToHandleAt),
    ] {
        assert_eq!(from_x86_64(x86), Some(call), "x86-64 {x86}");
        assert_eq!(from_aarch64(generic), Some(call), "AArch64 {generic}");
        assert_eq!(from_arm(eabi), Some(call), "ARMv7-A {eabi}");
    }
}

/// `splice` and `copy_file_range`, against `asm-x86/unistd_64.h`,
/// `asm-generic/unistd.h` and `asm-arm/unistd-common.h` as QEMU vendors them.
#[test]
fn splice_and_copy_file_range_match_the_kernel_tables() {
    for (x86, generic, eabi, call) in [
        (275, 76, 340, Syscall::Splice),
        (326, 285, 391, Syscall::CopyFileRange),
    ] {
        assert_eq!(from_x86_64(x86), Some(call), "x86-64 {x86}");
        assert_eq!(from_aarch64(generic), Some(call), "AArch64 {generic}");
        assert_eq!(from_arm(eabi), Some(call), "ARMv7-A {eabi}");
    }
}

/// `flock`, `readahead` and the System V message queue and semaphore calls,
/// against `asm-x86/unistd_64.h`, `asm-generic/unistd.h` and
/// `asm-arm/unistd-common.h` as QEMU vendors them. ARMv7-A has a time64 form
/// of `semtimedop` alone; the other calls here take no time.
#[test]
fn locking_readahead_and_ipc_calls_match_the_kernel_tables() {
    for (x86, generic, eabi, call) in [
        (73, 32, 143, Syscall::Flock),
        (187, 213, 225, Syscall::Readahead),
        (68, 186, 303, Syscall::Msgget),
        (69, 189, 301, Syscall::Msgsnd),
        (70, 188, 302, Syscall::Msgrcv),
        (71, 187, 304, Syscall::Msgctl),
        (64, 190, 299, Syscall::Semget),
        (65, 193, 298, Syscall::Semop),
        (66, 191, 300, Syscall::Semctl),
        (220, 192, 312, Syscall::Semtimedop),
        (67, 197, 306, Syscall::Shmdt),
    ] {
        assert_eq!(from_x86_64(x86), Some(call), "x86-64 {x86}");
        assert_eq!(from_aarch64(generic), Some(call), "AArch64 {generic}");
        assert_eq!(from_arm(eabi), Some(call), "ARMv7-A {eabi}");
    }
    assert_eq!(from_arm(420), Some(Syscall::SemtimedopTime64));
    assert_eq!(
        from_aarch64(420),
        None,
        "a 64-bit semtimedop is already 64-bit"
    );
}

// ---------------------------------------------------------------------------
// AT_HWCAP
//
// The register values are QEMU's reset values for the cores the boot test
// runs (`target/arm/tcg/cpu32.c` and `target/arm/cpu64.c`), so each expected
// set is what a program on that machine should be told.
// ---------------------------------------------------------------------------

#[test]
fn hwcap_bits_match_the_uapi_headers() {
    use crate::hwcap::{aarch64 as a64, arm};
    // Linux's `arch/arm/include/uapi/asm/hwcap.h` and
    // `arch/arm64/include/uapi/asm/hwcap.h`, read at torvalds/linux master on
    // 2026-09-13.
    for (bit, shift, name) in [
        (arm::HWCAP_HALF, 1, "HALF"),
        (arm::HWCAP_THUMB, 2, "THUMB"),
        (arm::HWCAP_FAST_MULT, 4, "FAST_MULT"),
        (arm::HWCAP_VFP, 6, "VFP"),
        (arm::HWCAP_EDSP, 7, "EDSP"),
        (arm::HWCAP_NEON, 12, "NEON"),
        (arm::HWCAP_VFPV3, 13, "VFPv3"),
        (arm::HWCAP_VFPV3D16, 14, "VFPv3D16"),
        (arm::HWCAP_TLS, 15, "TLS"),
        (arm::HWCAP_VFPV4, 16, "VFPv4"),
        (arm::HWCAP_IDIVA, 17, "IDIVA"),
        (arm::HWCAP_IDIVT, 18, "IDIVT"),
        (arm::HWCAP_VFPD32, 19, "VFPD32"),
        (arm::HWCAP_LPAE, 20, "LPAE"),
    ] {
        assert_eq!(bit, 1 << shift, "arm HWCAP_{name}");
    }
    for (bit, shift, name) in [
        (a64::HWCAP_FP, 0, "FP"),
        (a64::HWCAP_ASIMD, 1, "ASIMD"),
        (a64::HWCAP_AES, 3, "AES"),
        (a64::HWCAP_PMULL, 4, "PMULL"),
        (a64::HWCAP_SHA1, 5, "SHA1"),
        (a64::HWCAP_SHA2, 6, "SHA2"),
        (a64::HWCAP_CRC32, 7, "CRC32"),
        (a64::HWCAP_ATOMICS, 8, "ATOMICS"),
        (a64::HWCAP_FPHP, 9, "FPHP"),
        (a64::HWCAP_ASIMDHP, 10, "ASIMDHP"),
        (a64::HWCAP_ASIMDRDM, 12, "ASIMDRDM"),
        (a64::HWCAP_JSCVT, 13, "JSCVT"),
        (a64::HWCAP_FCMA, 14, "FCMA"),
        (a64::HWCAP_LRCPC, 15, "LRCPC"),
        (a64::HWCAP_SHA3, 17, "SHA3"),
        (a64::HWCAP_SM3, 18, "SM3"),
        (a64::HWCAP_SM4, 19, "SM4"),
        (a64::HWCAP_ASIMDDP, 20, "ASIMDDP"),
        (a64::HWCAP_SHA512, 21, "SHA512"),
        (a64::HWCAP_ASIMDFHM, 23, "ASIMDFHM"),
        (a64::HWCAP_ILRCPC, 26, "ILRCPC"),
        (a64::HWCAP_FLAGM, 27, "FLAGM"),
        (a64::HWCAP_SB, 29, "SB"),
        (a64::HWCAP2_FLAGM2, 7, "2_FLAGM2"),
        (a64::HWCAP2_FRINT, 8, "2_FRINT"),
        (a64::HWCAP2_I8MM, 13, "2_I8MM"),
        (a64::HWCAP2_BF16, 14, "2_BF16"),
        (a64::HWCAP2_RNG, 16, "2_RNG"),
    ] {
        assert_eq!(bit, 1 << shift, "arm64 HWCAP{name}");
    }
}

#[test]
fn a_cortex_a15_is_told_about_its_fpu_neon_and_divide() {
    use crate::hwcap::arm::{
        HWCAP_EDSP, HWCAP_FAST_MULT, HWCAP_HALF, HWCAP_IDIVA, HWCAP_IDIVT, HWCAP_LPAE, HWCAP_NEON,
        HWCAP_THUMB, HWCAP_TLS, HWCAP_VFP, HWCAP_VFPD32, HWCAP_VFPV3, HWCAP_VFPV4, IdRegisters,
        hwcap,
    };
    // QEMU's cortex-a15: MVFR0 0x10110222, MVFR1 0x11111111, ID_ISAR0
    // 0x02101110, ID_MMFR0 0x10201105. QEMU's cortex-a7 differs only in
    // ID_MMFR0 (0x10101105), which leaves VMSA at 5.
    let ids = IdRegisters {
        id_isar0: 0x0210_1110,
        id_mmfr0: 0x1020_1105,
        mvfr0: 0x1011_0222,
        mvfr1: 0x1111_1111,
    };
    let want = HWCAP_HALF
        | HWCAP_THUMB
        | HWCAP_FAST_MULT
        | HWCAP_EDSP
        | HWCAP_TLS
        | HWCAP_VFP
        | HWCAP_VFPV3
        | HWCAP_VFPD32
        | HWCAP_VFPV4
        | HWCAP_NEON
        | HWCAP_IDIVA
        | HWCAP_IDIVT
        | HWCAP_LPAE;
    assert_eq!(hwcap(ids), want);
    let a7 = IdRegisters {
        id_mmfr0: 0x1010_1105,
        ..ids
    };
    assert_eq!(hwcap(a7), want);
}

#[test]
fn an_arm_core_without_an_fpu_is_told_about_none() {
    use crate::hwcap::arm::{
        HWCAP_NEON, HWCAP_TLS, HWCAP_VFP, HWCAP_VFPD32, HWCAP_VFPV3, HWCAP_VFPV3D16, HWCAP_VFPV4,
        IdRegisters, hwcap,
    };
    let ids = IdRegisters {
        id_isar0: 0x0210_1110,
        id_mmfr0: 0x1020_1105,
        mvfr0: 0,
        mvfr1: 0,
    };
    let fpu = HWCAP_VFP | HWCAP_VFPV3 | HWCAP_VFPV3D16 | HWCAP_VFPD32 | HWCAP_VFPV4 | HWCAP_NEON;
    assert_eq!(hwcap(ids) & fpu, 0);
    assert_ne!(
        hwcap(ids) & HWCAP_TLS,
        0,
        "TPIDRURO is ARMv7-A's, FPU or not"
    );
}

#[test]
fn a_sixteen_register_vfpv3_is_d16_and_not_d32() {
    use crate::hwcap::arm::{
        HWCAP_IDIVA, HWCAP_NEON, HWCAP_VFP, HWCAP_VFPD32, HWCAP_VFPV3, HWCAP_VFPV3D16, HWCAP_VFPV4,
        IdRegisters, hwcap,
    };
    // A VFPv3-D16 without Advanced SIMD: SIMDReg 1, FPSP 2, FPDP 2.
    let ids = IdRegisters {
        mvfr0: 0x1011_0221,
        mvfr1: 0x0000_0011,
        ..IdRegisters::default()
    };
    let bits = hwcap(ids);
    assert_eq!(
        bits & (HWCAP_VFP | HWCAP_VFPV3 | HWCAP_VFPV3D16),
        HWCAP_VFP | HWCAP_VFPV3 | HWCAP_VFPV3D16
    );
    assert_eq!(
        bits & (HWCAP_VFPD32 | HWCAP_NEON | HWCAP_VFPV4 | HWCAP_IDIVA),
        0
    );
}

#[test]
fn a_cortex_a57_is_told_about_its_fp_simd_and_crypto() {
    use crate::hwcap::aarch64::{
        HWCAP_AES, HWCAP_ASIMD, HWCAP_CRC32, HWCAP_FP, HWCAP_PMULL, HWCAP_SHA1, HWCAP_SHA2,
        IdRegisters, hwcap, hwcap2,
    };
    // QEMU's cortex-a57: ID_AA64PFR0 0x2222, ID_AA64ISAR0 0x11120, ISAR1 0.
    let ids = IdRegisters {
        pfr0: 0x2222,
        isar0: 0x0001_1120,
        isar1: 0,
    };
    assert_eq!(
        hwcap(ids),
        HWCAP_FP | HWCAP_ASIMD | HWCAP_AES | HWCAP_PMULL | HWCAP_SHA1 | HWCAP_SHA2 | HWCAP_CRC32
    );
    assert_eq!(hwcap2(ids), 0);
}

#[test]
fn an_aarch64_core_without_fp_and_with_the_later_fields_is_told_exactly() {
    use crate::hwcap::aarch64::{
        HWCAP_FLAGM, HWCAP_ILRCPC, HWCAP_LRCPC, HWCAP_SHA2, HWCAP_SHA512, HWCAP2_BF16,
        HWCAP2_FLAGM2, HWCAP2_FRINT, HWCAP2_I8MM, HWCAP2_RNG, IdRegisters, hwcap, hwcap2,
    };
    // FP and AdvSIMD 0xF: absent. ID_AA64ISAR0: SHA2 2, TS 2, RNDR 1.
    // ID_AA64ISAR1: DPB 2, LRCPC 2, FRINTTS 1, BF16 1, I8MM 1. DPB grants
    // nothing: see the next test.
    let ids = IdRegisters {
        pfr0: 0x00FF_0000,
        isar0: 0x1020_0000_0000_2000,
        isar1: 0x0010_1001_0020_0002,
    };
    assert_eq!(
        hwcap(ids),
        HWCAP_SHA2 | HWCAP_SHA512 | HWCAP_FLAGM | HWCAP_LRCPC | HWCAP_ILRCPC
    );
    assert_eq!(
        hwcap2(ids),
        HWCAP2_FLAGM2 | HWCAP2_RNG | HWCAP2_FRINT | HWCAP2_BF16 | HWCAP2_I8MM
    );
}

#[test]
fn aarch64_fields_are_read_as_linux_reads_them() {
    use crate::hwcap::aarch64::{
        HWCAP_ASIMD, HWCAP_ASIMDHP, HWCAP_ATOMICS, HWCAP_FP, HWCAP_FPHP, IdRegisters, hwcap, hwcap2,
    };
    // ID_AA64PFR0.FP and .AdvSIMD are signed fields, and Linux's
    // `feature_matches` compares them signed: 0x8 to 0xE are negative and
    // grant nothing, not only 0xF.
    for negative in 0x8..=0xE_u64 {
        let ids = IdRegisters {
            pfr0: (negative << 20) | (negative << 16),
            ..IdRegisters::default()
        };
        assert_eq!(
            hwcap(ids) & (HWCAP_FP | HWCAP_FPHP | HWCAP_ASIMD | HWCAP_ASIMDHP),
            0,
            "FP and AdvSIMD {negative:#x}"
        );
    }
    // ID_AA64ISAR0.Atomic starts at 2 (`arch/arm64/tools/sysreg`: 0b0010 IMP),
    // and 1 is not LSE.
    let one = IdRegisters {
        pfr0: 0x00FF_0000,
        isar0: 1 << 20,
        ..IdRegisters::default()
    };
    assert_eq!(hwcap(one) & HWCAP_ATOMICS, 0);
    let two = IdRegisters {
        isar0: 2 << 20,
        ..one
    };
    assert_eq!(hwcap(two) & HWCAP_ATOMICS, HWCAP_ATOMICS);
    // DPB 2 is `DC CVAP` and `DC CVADP`, which trap from EL0 unless
    // SCTLR_EL1.UCI is set, and nothing here sets it.
    let dpb = IdRegisters {
        pfr0: 0x00FF_0000,
        isar1: 2,
        ..IdRegisters::default()
    };
    assert_eq!(hwcap(dpb), 0);
    assert_eq!(hwcap2(dpb), 0);
}

#[test]
fn arm_fields_are_read_as_linux_reads_them() {
    use crate::hwcap::arm::{
        HWCAP_EDSP, HWCAP_FAST_MULT, HWCAP_HALF, HWCAP_NEON, HWCAP_THUMB, HWCAP_TLS, HWCAP_VFP,
        HWCAP_VFPD32, HWCAP_VFPV3, HWCAP_VFPV4, IdRegisters, hwcap,
    };
    let base = HWCAP_HALF | HWCAP_THUMB | HWCAP_FAST_MULT | HWCAP_EDSP | HWCAP_TLS;
    // `cpuid_feature_extract_field` is signed: a Divide or VMSA field of 0x8
    // to 0xF is negative, so no divide and no LPAE.
    for negative in 0x8..=0xF_u32 {
        let ids = IdRegisters {
            id_isar0: negative << 24,
            id_mmfr0: negative,
            ..IdRegisters::default()
        };
        assert_eq!(hwcap(ids), base, "Divide and VMSA {negative:#x}");
    }
    // `vfp_init` tests equality: VFPv3 is FPSP or FPDP exactly 2, D16 is
    // SIMDReg exactly 1 and anything else D32, NEON is SIMDLS, SIMDInt and
    // SIMDSP all exactly 1, and VFPv4 is SIMDFMAC exactly 1.
    let beyond = IdRegisters {
        mvfr0: 0x0000_0330,
        mvfr1: 0x2002_2200,
        ..IdRegisters::default()
    };
    assert_eq!(hwcap(beyond), base | HWCAP_VFP);
    let no_simd_registers = IdRegisters {
        mvfr0: 0x0000_0220,
        mvfr1: 0,
        ..IdRegisters::default()
    };
    assert_eq!(
        hwcap(no_simd_registers),
        base | HWCAP_VFP | HWCAP_VFPV3 | HWCAP_VFPD32
    );
    let exact = IdRegisters {
        mvfr0: 0x0000_0222,
        mvfr1: 0x1001_1100,
        ..IdRegisters::default()
    };
    assert_eq!(
        hwcap(exact),
        base | HWCAP_VFP | HWCAP_VFPV3 | HWCAP_VFPD32 | HWCAP_NEON | HWCAP_VFPV4
    );
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

use crate::socket::{
    self, AddressError, BadControlMessage, CmsgHdr, ControlMessage, ControlMessages, Linger,
    MsgHdr, SOCKADDR_UN_SIZE, Ucred, UnixAddress, Width, cmsg_align, cmsg_len, cmsg_space,
};

#[test]
fn socket_families_types_and_shutdown_match_the_headers() {
    for (value, expected, name) in [
        (socket::AF_UNSPEC, 0, "AF_UNSPEC"),
        (socket::AF_UNIX, 1, "AF_UNIX"),
        (socket::AF_LOCAL, 1, "AF_LOCAL"),
        (socket::AF_INET, 2, "AF_INET"),
        (socket::AF_INET6, 10, "AF_INET6"),
        (socket::AF_NETLINK, 16, "AF_NETLINK"),
        (socket::AF_MAX, 46, "AF_MAX"),
    ] {
        assert_eq!(value, expected, "{name}");
    }
    for (value, expected, name) in [
        (socket::SOCK_STREAM, 1, "SOCK_STREAM"),
        (socket::SOCK_DGRAM, 2, "SOCK_DGRAM"),
        (socket::SOCK_RAW, 3, "SOCK_RAW"),
        (socket::SOCK_RDM, 4, "SOCK_RDM"),
        (socket::SOCK_SEQPACKET, 5, "SOCK_SEQPACKET"),
        (socket::SOCK_DCCP, 6, "SOCK_DCCP"),
        (socket::SOCK_PACKET, 10, "SOCK_PACKET"),
        (socket::SOCK_TYPE_MASK, 0xf, "SOCK_TYPE_MASK"),
        (socket::SOCK_NONBLOCK, 0o4000, "SOCK_NONBLOCK"),
        (socket::SOCK_CLOEXEC, 0o2000000, "SOCK_CLOEXEC"),
        (socket::SHUT_RD, 0, "SHUT_RD"),
        (socket::SHUT_WR, 1, "SHUT_WR"),
        (socket::SHUT_RDWR, 2, "SHUT_RDWR"),
    ] {
        assert_eq!(value, expected, "{name}");
    }
    assert_eq!(
        (socket::SOCK_NONBLOCK | socket::SOCK_CLOEXEC) & socket::SOCK_TYPE_MASK,
        0,
        "the creation flags must be clear of the type"
    );
}

#[test]
fn message_flags_are_distinct_bits_with_the_header_values() {
    let flags = [
        (socket::MSG_OOB, 0x1, "MSG_OOB"),
        (socket::MSG_PEEK, 0x2, "MSG_PEEK"),
        (socket::MSG_DONTROUTE, 0x4, "MSG_DONTROUTE"),
        (socket::MSG_CTRUNC, 0x8, "MSG_CTRUNC"),
        (socket::MSG_PROXY, 0x10, "MSG_PROXY"),
        (socket::MSG_TRUNC, 0x20, "MSG_TRUNC"),
        (socket::MSG_DONTWAIT, 0x40, "MSG_DONTWAIT"),
        (socket::MSG_EOR, 0x80, "MSG_EOR"),
        (socket::MSG_WAITALL, 0x100, "MSG_WAITALL"),
        (socket::MSG_FIN, 0x200, "MSG_FIN"),
        (socket::MSG_SYN, 0x400, "MSG_SYN"),
        (socket::MSG_CONFIRM, 0x800, "MSG_CONFIRM"),
        (socket::MSG_RST, 0x1000, "MSG_RST"),
        (socket::MSG_ERRQUEUE, 0x2000, "MSG_ERRQUEUE"),
        (socket::MSG_NOSIGNAL, 0x4000, "MSG_NOSIGNAL"),
        (socket::MSG_MORE, 0x8000, "MSG_MORE"),
        (socket::MSG_WAITFORONE, 0x10000, "MSG_WAITFORONE"),
        (socket::MSG_BATCH, 0x40000, "MSG_BATCH"),
        (socket::MSG_ZEROCOPY, 0x4000000, "MSG_ZEROCOPY"),
        (socket::MSG_FASTOPEN, 0x20000000, "MSG_FASTOPEN"),
        (socket::MSG_CMSG_CLOEXEC, 0x40000000, "MSG_CMSG_CLOEXEC"),
        (socket::MSG_CMSG_COMPAT, 0x80000000, "MSG_CMSG_COMPAT"),
    ];
    let mut seen = 0_u32;
    for (value, expected, name) in flags {
        assert_eq!(value, expected, "{name}");
        assert_eq!(value.count_ones(), 1, "{name} is one bit");
        assert_eq!(seen & value, 0, "{name} shares a bit");
        seen |= value;
    }
}

#[test]
fn socket_options_match_the_generic_header() {
    assert_eq!(socket::SOL_SOCKET, 1, "SOL_SOCKET");
    for (value, expected, name) in [
        (socket::SO_DEBUG, 1, "SO_DEBUG"),
        (socket::SO_REUSEADDR, 2, "SO_REUSEADDR"),
        (socket::SO_TYPE, 3, "SO_TYPE"),
        (socket::SO_ERROR, 4, "SO_ERROR"),
        (socket::SO_DONTROUTE, 5, "SO_DONTROUTE"),
        (socket::SO_BROADCAST, 6, "SO_BROADCAST"),
        (socket::SO_SNDBUF, 7, "SO_SNDBUF"),
        (socket::SO_RCVBUF, 8, "SO_RCVBUF"),
        (socket::SO_KEEPALIVE, 9, "SO_KEEPALIVE"),
        (socket::SO_OOBINLINE, 10, "SO_OOBINLINE"),
        (socket::SO_NO_CHECK, 11, "SO_NO_CHECK"),
        (socket::SO_PRIORITY, 12, "SO_PRIORITY"),
        (socket::SO_LINGER, 13, "SO_LINGER"),
        (socket::SO_BSDCOMPAT, 14, "SO_BSDCOMPAT"),
        (socket::SO_REUSEPORT, 15, "SO_REUSEPORT"),
        (socket::SO_PASSCRED, 16, "SO_PASSCRED"),
        (socket::SO_PEERCRED, 17, "SO_PEERCRED"),
        (socket::SO_RCVLOWAT, 18, "SO_RCVLOWAT"),
        (socket::SO_SNDLOWAT, 19, "SO_SNDLOWAT"),
        (socket::SO_RCVTIMEO_OLD, 20, "SO_RCVTIMEO_OLD"),
        (socket::SO_SNDTIMEO_OLD, 21, "SO_SNDTIMEO_OLD"),
        (socket::SO_ACCEPTCONN, 30, "SO_ACCEPTCONN"),
        (socket::SO_PEERSEC, 31, "SO_PEERSEC"),
        (socket::SO_SNDBUFFORCE, 32, "SO_SNDBUFFORCE"),
        (socket::SO_RCVBUFFORCE, 33, "SO_RCVBUFFORCE"),
        (socket::SO_PASSSEC, 34, "SO_PASSSEC"),
        (socket::SO_PROTOCOL, 38, "SO_PROTOCOL"),
        (socket::SO_DOMAIN, 39, "SO_DOMAIN"),
        (socket::SO_PEEK_OFF, 42, "SO_PEEK_OFF"),
        (socket::SO_PEERGROUPS, 59, "SO_PEERGROUPS"),
        (socket::SO_RCVTIMEO_NEW, 66, "SO_RCVTIMEO_NEW"),
        (socket::SO_SNDTIMEO_NEW, 67, "SO_SNDTIMEO_NEW"),
        (socket::SO_PASSPIDFD, 76, "SO_PASSPIDFD"),
        (socket::SO_PEERPIDFD, 77, "SO_PEERPIDFD"),
        (socket::SCM_RIGHTS, 1, "SCM_RIGHTS"),
        (socket::SCM_CREDENTIALS, 2, "SCM_CREDENTIALS"),
    ] {
        assert_eq!(value, expected, "{name}");
    }
}

#[test]
fn socket_limits_and_queue_ioctls_match_linux() {
    assert_eq!(socket::SCM_MAX_FD, 253, "SCM_MAX_FD");
    assert_eq!(socket::UNIX_PATH_MAX, 108, "UNIX_PATH_MAX");
    assert_eq!(SOCKADDR_UN_SIZE, 110, "sizeof(struct sockaddr_un)");
    assert_eq!(socket::SOMAXCONN, 4096, "net.core.somaxconn since 5.4");
    assert_eq!(socket::SOCKET_BUFFER_DEFAULT, 212_992, "rmem_default");
    assert_eq!(socket::SOCKET_BUFFER_MAX, 212_992, "rmem_max");
    assert_eq!(socket::SOCKET_BUFFER_MIN, 4096, "one page");
    assert_eq!(types::TIOCOUTQ, 0x5411, "TIOCOUTQ is 'T' 0x11");
    assert_eq!(socket::SIOCINQ, 0x541B, "SIOCINQ is FIONREAD");
    assert_eq!(socket::SIOCOUTQ, 0x5411, "SIOCOUTQ is TIOCOUTQ");
}

#[test]
fn msghdr_is_seven_words_with_every_field_on_a_word_boundary() {
    // Independent transcriptions of the kernel's `struct user_msghdr`, with
    // each architecture's `size_t` and pointer.
    #[repr(C)]
    struct Wide {
        name: u64,
        namelen: u32,
        iov: u64,
        iovlen: u64,
        control: u64,
        controllen: u64,
        flags: u32,
    }
    #[repr(C)]
    struct Narrow {
        name: u32,
        namelen: u32,
        iov: u32,
        iovlen: u32,
        control: u32,
        controllen: u32,
        flags: u32,
    }
    let wide = [
        offset_of!(Wide, namelen),
        offset_of!(Wide, iov),
        offset_of!(Wide, iovlen),
        offset_of!(Wide, control),
        offset_of!(Wide, controllen),
        offset_of!(Wide, flags),
    ];
    let narrow = [
        offset_of!(Narrow, namelen),
        offset_of!(Narrow, iov),
        offset_of!(Narrow, iovlen),
        offset_of!(Narrow, control),
        offset_of!(Narrow, controllen),
        offset_of!(Narrow, flags),
    ];
    assert_eq!(offset_of!(Wide, name), 0);
    assert_eq!(offset_of!(Narrow, name), 0);
    for (width, size, model_size, model, literal) in [
        (
            Width::Bits64,
            56,
            size_of::<Wide>(),
            wide,
            [8, 16, 24, 32, 40, 48],
        ),
        (
            Width::Bits32,
            28,
            size_of::<Narrow>(),
            narrow,
            [4, 8, 12, 16, 20, 24],
        ),
    ] {
        let offsets = [
            MsgHdr::name_len_offset(width),
            MsgHdr::iov_offset(width),
            MsgHdr::iov_len_offset(width),
            MsgHdr::control_offset(width),
            MsgHdr::control_len_offset(width),
            MsgHdr::flags_offset(width),
        ];
        assert_eq!(MsgHdr::size(width), size, "{width:?} size");
        assert_eq!(model_size, size, "{width:?} model size");
        assert_eq!(offsets, literal, "{width:?} offsets");
        assert_eq!(offsets, model, "{width:?} offsets against the model");
    }
}

#[test]
fn msghdr_decodes_at_the_kernel_offsets_and_encodes_its_padding_zero() {
    let expected = MsgHdr {
        name: 0x1122_3344_5566_7788,
        name_len: 110,
        iov: 0x0000_7FFF_0000_1000,
        iov_len: 3,
        control: 0x0000_7FFF_0000_2000,
        control_len: 64,
        flags: socket::MSG_TRUNC,
    };
    // The 64-bit layout, with its padding zeroed as musl leaves it.
    let mut wide = [0_u8; 56];
    wide[0..8].copy_from_slice(&expected.name.to_le_bytes());
    wide[8..12].copy_from_slice(&110_u32.to_le_bytes());
    wide[16..24].copy_from_slice(&expected.iov.to_le_bytes());
    wide[24..32].copy_from_slice(&3_u64.to_le_bytes());
    wide[32..40].copy_from_slice(&expected.control.to_le_bytes());
    wide[40..48].copy_from_slice(&64_u64.to_le_bytes());
    wide[48..52].copy_from_slice(&socket::MSG_TRUNC.to_le_bytes());
    assert_eq!(MsgHdr::decode(&wide, Width::Bits64), Some(expected));
    assert_eq!(MsgHdr::decode(&wide[..55], Width::Bits64), None);

    let mut out = [0xFF_u8; 60];
    expected.encode(&mut out, Width::Bits64).unwrap();
    assert_eq!(&out[..56], &wide[..], "padding must be written zero");
    assert_eq!(&out[56..], &[0xFF; 4], "nothing past the structure");

    let mut narrow = [0_u8; 28];
    for (at, value) in [
        (0, 0x1000_u32),
        (4, 16),
        (8, 0x2000),
        (12, 2),
        (16, 0x3000),
        (20, 24),
        (24, 0x80),
    ] {
        narrow[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    let short = MsgHdr {
        name: 0x1000,
        name_len: 16,
        iov: 0x2000,
        iov_len: 2,
        control: 0x3000,
        control_len: 24,
        flags: socket::MSG_EOR,
    };
    assert_eq!(MsgHdr::decode(&narrow, Width::Bits32), Some(short));
    let mut out = [0xFF_u8; 28];
    short.encode(&mut out, Width::Bits32).unwrap();
    assert_eq!(out, narrow);
    assert_eq!(short.encode(&mut out[..27], Width::Bits32), None);
}

#[test]
fn msghdr_round_trips_on_both_widths_and_refuses_what_does_not_fit() {
    let header = MsgHdr {
        name: 0xBEEF,
        name_len: 3,
        iov: 0xCAFE,
        iov_len: 1024,
        control: 0xF00D,
        control_len: 1032,
        flags: socket::MSG_CTRUNC | socket::MSG_TRUNC,
    };
    for width in [Width::Bits64, Width::Bits32] {
        let mut bytes = [0_u8; 56];
        header.encode(&mut bytes, width).unwrap();
        assert_eq!(MsgHdr::decode(&bytes, width), Some(header), "{width:?}");
        // What the kernel writes back after `recvmsg`, one field at a time.
        bytes[MsgHdr::name_len_offset(width)..][..4].copy_from_slice(&7_u32.to_le_bytes());
        width
            .put_word(&mut bytes, MsgHdr::control_len_offset(width), 20)
            .unwrap();
        bytes[MsgHdr::flags_offset(width)..][..4].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(
            MsgHdr::decode(&bytes, width),
            Some(MsgHdr {
                name_len: 7,
                control_len: 20,
                flags: 0,
                ..header
            }),
            "{width:?}"
        );
    }
    let wide = MsgHdr {
        control_len: 1 << 32,
        ..header
    };
    let mut out = [0x55_u8; 28];
    assert_eq!(wide.encode(&mut out, Width::Bits32), None);
    assert_eq!(out, [0x55; 28], "a refused encode writes nothing");
    assert_eq!(Width::Bits32.put_word(&mut out, 0, 1 << 32), None);
    assert_eq!(Width::Bits64.put_word(&mut out, 24, 1), None);
}

#[test]
fn cmsghdr_is_a_word_and_two_ints() {
    for (width, size, level, kind) in [(Width::Bits64, 16, 8, 12), (Width::Bits32, 12, 4, 8)] {
        assert_eq!(CmsgHdr::size(width), size, "{width:?}");
        assert_eq!(CmsgHdr::level_offset(width), level, "{width:?}");
        assert_eq!(CmsgHdr::kind_offset(width), kind, "{width:?}");
        let header = CmsgHdr {
            len: 20,
            level: socket::SOL_SOCKET,
            kind: -2,
        };
        let mut bytes = [0xAA_u8; 16];
        header.encode(&mut bytes, width).unwrap();
        assert_eq!(
            &bytes[..width.bytes()],
            &20_u64.to_le_bytes()[..width.bytes()]
        );
        assert_eq!(&bytes[level..level + 4], &1_i32.to_le_bytes());
        assert_eq!(&bytes[kind..kind + 4], &(-2_i32).to_le_bytes());
        assert_eq!(CmsgHdr::decode(&bytes, width), Some(header));
        assert_eq!(CmsgHdr::decode(&bytes[..size - 1], width), None);
        assert_eq!(header.encode(&mut bytes[..size - 1], width), None);
    }
}

#[test]
fn cmsg_arithmetic_matches_the_musl_macros() {
    // musl: `CMSG_ALIGN(len)` is `(len + sizeof(size_t) - 1) & ~(sizeof(size_t)
    // - 1)`, `CMSG_SPACE(len)` is `CMSG_ALIGN(len) + CMSG_ALIGN(sizeof(struct
    // cmsghdr))` and `CMSG_LEN(len)` is `CMSG_ALIGN(sizeof(struct cmsghdr)) +
    // len`.
    let align = |len: usize, word: usize| (len + word - 1) & !(word - 1);
    let most_rights = 4 * socket::SCM_MAX_FD;
    for (width, word, header) in [(Width::Bits64, 8, 16), (Width::Bits32, 4, 12)] {
        for len in [0, 1, 4, 8, 12, most_rights] {
            assert_eq!(cmsg_align(len, width), align(len, word), "{width:?} {len}");
            assert_eq!(
                cmsg_space(len, width),
                align(len, word) + align(header, word),
                "{width:?} {len}"
            );
            assert_eq!(
                cmsg_len(len, width),
                align(header, word) + len,
                "{width:?} {len}"
            );
        }
    }
    // The same numbers written out: (data, CMSG_ALIGN, CMSG_SPACE, CMSG_LEN).
    let wide = [
        (1, 8, 24, 17),
        (4, 8, 24, 20),
        (8, 8, 24, 24),
        (12, 16, 32, 28),
        (1012, 1016, 1032, 1028),
    ];
    let narrow = [
        (1, 4, 16, 13),
        (4, 4, 16, 16),
        (8, 8, 20, 20),
        (12, 12, 24, 24),
        (1012, 1012, 1024, 1024),
    ];
    for (width, table) in [(Width::Bits64, wide), (Width::Bits32, narrow)] {
        for (len, aligned, space, total) in table {
            assert_eq!(cmsg_align(len, width), aligned, "{width:?} {len}");
            assert_eq!(cmsg_space(len, width), space, "{width:?} {len}");
            assert_eq!(cmsg_len(len, width), total, "{width:?} {len}");
        }
    }
    assert!(
        cmsg_align(usize::MAX - 2, Width::Bits64) >= usize::MAX - 2,
        "aligning saturates rather than wrapping"
    );
}

/// A control buffer holding an `SCM_RIGHTS` message with one descriptor and
/// an `SCM_CREDENTIALS` one, each at its `CMSG_SPACE`, followed by `tail`
/// spare bytes.
fn two_control_messages(width: Width, cred: Ucred, tail: usize) -> Vec<u8> {
    let header = CmsgHdr::size(width);
    let second = cmsg_space(4, width);
    let mut control = std::vec![0_u8; second + cmsg_space(Ucred::SIZE, width) + tail];
    CmsgHdr {
        len: u64::try_from(cmsg_len(4, width)).unwrap(),
        level: socket::SOL_SOCKET,
        kind: socket::SCM_RIGHTS,
    }
    .encode(&mut control, width)
    .unwrap();
    control[header..header + 4].copy_from_slice(&7_i32.to_le_bytes());
    CmsgHdr {
        len: u64::try_from(cmsg_len(Ucred::SIZE, width)).unwrap(),
        level: socket::SOL_SOCKET,
        kind: socket::SCM_CREDENTIALS,
    }
    .encode(&mut control[second..], width)
    .unwrap();
    control[second + header..second + header + Ucred::SIZE].copy_from_slice(&cred.to_bytes());
    control
}

#[test]
fn control_messages_are_walked_as_for_each_cmsghdr_walks_them() {
    let cred = Ucred {
        pid: 42,
        uid: 1000,
        gid: 100,
    };
    let rights = 7_i32.to_le_bytes();
    let cred_bytes = cred.to_bytes();
    for width in [Width::Bits64, Width::Bits32] {
        let expected = [
            Ok(ControlMessage {
                level: socket::SOL_SOCKET,
                kind: socket::SCM_RIGHTS,
                data: &rights[..],
            }),
            Ok(ControlMessage {
                level: socket::SOL_SOCKET,
                kind: socket::SCM_CREDENTIALS,
                data: &cred_bytes[..],
            }),
        ];
        // Spare bytes too few for a header are not a message.
        let control = two_control_messages(width, cred, CmsgHdr::size(width) - 1);
        let found: Vec<_> = ControlMessages::new(&control, width).collect();
        assert_eq!(found, expected, "{width:?}");
        // Without the last message's padding the walk still finds it:
        // `CMSG_OK` measures `cmsg_len`, not `CMSG_SPACE`.
        let unpadded = cmsg_space(4, width) + cmsg_len(Ucred::SIZE, width);
        let found: Vec<_> = ControlMessages::new(&control[..unpadded], width).collect();
        assert_eq!(found, expected, "{width:?} unpadded");
        assert_eq!(ControlMessages::new(&[], width).count(), 0);
        assert_eq!(
            ControlMessages::new(&control[..CmsgHdr::size(width) - 1], width).count(),
            0,
            "{width:?}: a buffer shorter than a header has no first message"
        );
    }
}

#[test]
fn a_control_message_whose_length_is_wrong_ends_the_walk_with_an_error() {
    let cred = Ucred::default();
    for width in [Width::Bits64, Width::Bits32] {
        let header = CmsgHdr::size(width);
        let second = cmsg_space(4, width);
        for bad_len in [0, header as u64 - 1, 1 << 20] {
            let mut control = two_control_messages(width, cred, 0);
            width.put_word(&mut control, second, bad_len).unwrap();
            let found: Vec<_> = ControlMessages::new(&control, width).collect();
            assert_eq!(found.len(), 2, "{width:?} {bad_len}");
            assert!(found[0].is_ok(), "{width:?} {bad_len}");
            assert_eq!(found[1], Err(BadControlMessage), "{width:?} {bad_len}");
        }
        // A length one past the buffer is refused; exactly the buffer is not.
        let mut control = two_control_messages(width, cred, 0);
        let rest = u64::try_from(control.len() - second).unwrap();
        width.put_word(&mut control, second, rest + 1).unwrap();
        assert_eq!(
            ControlMessages::new(&control, width).nth(1),
            Some(Err(BadControlMessage))
        );
        width.put_word(&mut control, second, rest).unwrap();
        assert!(matches!(
            ControlMessages::new(&control, width).nth(1),
            Some(Ok(_))
        ));
    }
}

/// A `struct sockaddr_un` of family `family` with `name` at `sun_path`, in a
/// buffer longer than the structure so a too-long length can be tried.
fn sockaddr_un(family: u16, name: &[u8]) -> [u8; 112] {
    let mut bytes = [0_u8; 112];
    bytes[..2].copy_from_slice(&family.to_le_bytes());
    bytes[2..2 + name.len()].copy_from_slice(name);
    bytes
}

#[test]
fn a_unix_address_is_told_apart_by_its_length_and_first_byte() {
    let unix = socket::AF_UNIX;
    assert_eq!(
        UnixAddress::parse(&sockaddr_un(unix, b""), 2),
        Ok(UnixAddress::Unnamed)
    );
    let path = sockaddr_un(unix, b"/run/x.sock");
    let named = Ok(UnixAddress::Path(b"/run/x.sock"));
    assert_eq!(UnixAddress::parse(&path, 2 + 11), named, "no terminator");
    assert_eq!(UnixAddress::parse(&path, 2 + 12), named, "with its NUL");
    assert_eq!(
        UnixAddress::parse(&path, SOCKADDR_UN_SIZE),
        named,
        "the whole structure, as most programs pass it"
    );
    assert_eq!(
        UnixAddress::parse(&sockaddr_un(unix, b"/a\0bc"), 7),
        Ok(UnixAddress::Path(b"/a")),
        "a path stops at its first NUL within the length"
    );
    let full = [b'p'; 108];
    assert_eq!(
        UnixAddress::parse(&sockaddr_un(unix, &full), SOCKADDR_UN_SIZE),
        Ok(UnixAddress::Path(&full)),
        "108 bytes need no terminator"
    );
    let hidden = sockaddr_un(unix, b"\0name\0with\0nuls");
    assert_eq!(
        UnixAddress::parse(&hidden, 2 + 15),
        Ok(UnixAddress::Abstract(b"name\0with\0nuls"))
    );
    assert_eq!(
        UnixAddress::parse(&hidden, 3),
        Ok(UnixAddress::Abstract(b""))
    );
    assert_eq!(
        UnixAddress::parse(&hidden, SOCKADDR_UN_SIZE),
        Ok(UnixAddress::Abstract(&hidden[3..110])),
        "the length, not a terminator, ends an abstract name"
    );
}

#[test]
fn a_unix_address_of_the_wrong_length_or_family_is_refused() {
    let unix = sockaddr_un(socket::AF_UNIX, b"/s");
    assert_eq!(UnixAddress::parse(&unix, 0), Err(AddressError::TooShort));
    assert_eq!(UnixAddress::parse(&unix, 1), Err(AddressError::TooShort));
    assert_eq!(
        UnixAddress::parse(&unix, SOCKADDR_UN_SIZE + 1),
        Err(AddressError::TooLong)
    );
    assert_eq!(
        UnixAddress::parse(&unix[..3], 4),
        Err(AddressError::TooShort),
        "fewer bytes than the length claims"
    );
    assert_eq!(
        UnixAddress::parse(&sockaddr_un(socket::AF_INET, b"/s"), 4),
        Err(AddressError::WrongFamily)
    );
    assert_eq!(
        UnixAddress::parse(&sockaddr_un(0x0101, b"/s"), 4),
        Err(AddressError::WrongFamily),
        "the family is sixteen bits, not one byte"
    );
    assert_eq!(
        UnixAddress::parse(&sockaddr_un(socket::AF_UNSPEC, b""), 2),
        Err(AddressError::WrongFamily)
    );
}

#[test]
fn a_unix_address_encodes_to_what_parses_back_to_it() {
    let full = [b'p'; 108];
    let longest_abstract = [1_u8; 107];
    for (address, len) in [
        (UnixAddress::Unnamed, 2),
        (UnixAddress::Path(b"/run/x.sock"), 14),
        (UnixAddress::Path(&full), 110),
        (UnixAddress::Abstract(b"name\0x"), 9),
        (UnixAddress::Abstract(b""), 3),
        (UnixAddress::Abstract(&longest_abstract), 110),
    ] {
        let mut out = [0xEE_u8; SOCKADDR_UN_SIZE];
        assert_eq!(address.encoded_len(), len, "{address:?}");
        assert_eq!(address.encode(&mut out), Some(len), "{address:?}");
        assert_eq!(&out[..2], &socket::AF_UNIX.to_le_bytes(), "{address:?}");
        assert_eq!(UnixAddress::parse(&out, len), Ok(address), "{address:?}");
        assert!(out[len..].iter().all(|&byte| byte == 0xEE), "{address:?}");
        if let UnixAddress::Path(path) = address
            && path.len() < 108
        {
            assert_eq!(out[len - 1], 0, "a short path is terminated");
        }
        assert_eq!(address.encode(&mut out[..len - 1]), None, "{address:?}");
    }
    let mut out = [0xEE_u8; SOCKADDR_UN_SIZE + 8];
    for invalid in [
        UnixAddress::Path(b""),
        UnixAddress::Path(b"a\0b"),
        UnixAddress::Path(&[b'p'; 109]),
        UnixAddress::Abstract(&[1; 108]),
    ] {
        assert_eq!(invalid.encode(&mut out), None, "{invalid:?}");
    }
    assert!(out.iter().all(|&byte| byte == 0xEE), "nothing written");
}

#[test]
fn ucred_and_linger_are_plain_ints_on_every_architecture() {
    assert_eq!(size_of::<Ucred>(), Ucred::SIZE);
    assert_eq!(Ucred::SIZE, 12);
    assert_eq!(offset_of!(Ucred, uid), 4);
    assert_eq!(offset_of!(Ucred, gid), 8);
    assert_eq!(size_of::<Linger>(), Linger::SIZE);
    assert_eq!(Linger::SIZE, 8);
    assert_eq!(offset_of!(Linger, linger), 4);

    let cred = Ucred {
        pid: 1234,
        uid: 1000,
        gid: 100,
    };
    let bytes = cred.to_bytes();
    assert_eq!(&bytes[0..4], &1234_i32.to_le_bytes());
    assert_eq!(&bytes[4..8], &1000_u32.to_le_bytes());
    assert_eq!(&bytes[8..12], &100_u32.to_le_bytes());
    assert_eq!(Ucred::from_bytes(&bytes), Some(cred));
    assert_eq!(Ucred::from_bytes(&bytes[..11]), None);

    let linger = Linger {
        onoff: 1,
        linger: -30,
    };
    let bytes = linger.to_bytes();
    assert_eq!(&bytes[4..8], &(-30_i32).to_le_bytes());
    assert_eq!(Linger::from_bytes(&bytes), Some(linger));
    assert_eq!(Linger::from_bytes(&bytes[..7]), None);
}

mod drm;
mod inet;
mod input;
mod netlink;
mod sound;
mod virtgpu;

/// Every i386 number [`from_i386`] translates beside the thread-area pair:
/// the constant, the number `asm/unistd_32.h` gives it, and ARMv7-A's constant
/// for the same call.
const I386_NUMBERS: [(usize, usize, usize); 240] = [
    (i386::FORK, 2, arm::FORK),
    (i386::KILL, 37, arm::KILL),
    (i386::WAIT4, 114, arm::WAIT4),
    (i386::SIGRETURN, 119, arm::SIGRETURN),
    (i386::CLONE, 120, arm::CLONE),
    (i386::RT_SIGRETURN, 173, arm::RT_SIGRETURN),
    (i386::RT_SIGACTION, 174, arm::RT_SIGACTION),
    (i386::RT_SIGPROCMASK, 175, arm::RT_SIGPROCMASK),
    (i386::RT_SIGSUSPEND, 179, arm::RT_SIGSUSPEND),
    (i386::SIGALTSTACK, 186, arm::SIGALTSTACK),
    (i386::VFORK, 190, arm::VFORK),
    (i386::TKILL, 238, arm::TKILL),
    (i386::TGKILL, 270, arm::TGKILL),
    (i386::RESTART_SYSCALL, 0, arm::RESTART_SYSCALL),
    (i386::EXIT, 1, arm::EXIT),
    (i386::READ, 3, arm::READ),
    (i386::WRITE, 4, arm::WRITE),
    (i386::CLOSE, 6, arm::CLOSE),
    (i386::GETPID, 20, arm::GETPID),
    (i386::DUP, 41, arm::DUP),
    (i386::DUP2, 63, arm::DUP2),
    (i386::GETPPID, 64, arm::GETPPID),
    (i386::UNAME, 122, arm::UNAME),
    (i386::SCHED_YIELD, 158, arm::SCHED_YIELD),
    (i386::GETUID32, 199, arm::GETUID32),
    (i386::GETGID32, 200, arm::GETGID32),
    (i386::GETEUID32, 201, arm::GETEUID32),
    (i386::GETEGID32, 202, arm::GETEGID32),
    (i386::GETTID, 224, arm::GETTID),
    (i386::EXIT_GROUP, 252, arm::EXIT_GROUP),
    (i386::DUP3, 330, arm::DUP3),
    (i386::OPEN, 5, arm::OPEN),
    (i386::CREAT, 8, arm::CREAT),
    (i386::LINK, 9, arm::LINK),
    (i386::UNLINK, 10, arm::UNLINK),
    (i386::EXECVE, 11, arm::EXECVE),
    (i386::CHDIR, 12, arm::CHDIR),
    (i386::MKNOD, 14, arm::MKNOD),
    (i386::CHMOD, 15, arm::CHMOD),
    (i386::LSEEK, 19, arm::LSEEK),
    (i386::MOUNT, 21, arm::MOUNT),
    (i386::ACCESS, 33, arm::ACCESS),
    (i386::SYNC, 36, arm::SYNC),
    (i386::RENAME, 38, arm::RENAME),
    (i386::MKDIR, 39, arm::MKDIR),
    (i386::RMDIR, 40, arm::RMDIR),
    (i386::PIPE, 42, arm::PIPE),
    (i386::BRK, 45, arm::BRK),
    (i386::UMOUNT2, 52, arm::UMOUNT2),
    (i386::IOCTL, 54, arm::IOCTL),
    (i386::FCNTL, 55, arm::FCNTL),
    (i386::SETPGID, 57, arm::SETPGID),
    (i386::UMASK, 60, arm::UMASK),
    (i386::CHROOT, 61, arm::CHROOT),
    (i386::GETPGRP, 65, arm::GETPGRP),
    (i386::SETSID, 66, arm::SETSID),
    (i386::SETHOSTNAME, 74, arm::SETHOSTNAME),
    (i386::SYMLINK, 83, arm::SYMLINK),
    (i386::READLINK, 85, arm::READLINK),
    (i386::MUNMAP, 91, arm::MUNMAP),
    (i386::TRUNCATE, 92, arm::TRUNCATE),
    (i386::FTRUNCATE, 93, arm::FTRUNCATE),
    (i386::FCHMOD, 94, arm::FCHMOD),
    (i386::GETPRIORITY, 96, arm::GETPRIORITY),
    (i386::SETPRIORITY, 97, arm::SETPRIORITY),
    (i386::FSYNC, 118, arm::FSYNC),
    (i386::SETDOMAINNAME, 121, arm::SETDOMAINNAME),
    (i386::MPROTECT, 125, arm::MPROTECT),
    (i386::GETPGID, 132, arm::GETPGID),
    (i386::FCHDIR, 133, arm::FCHDIR),
    (i386::LLSEEK, 140, arm::LLSEEK),
    (i386::FLOCK, 143, arm::FLOCK),
    (i386::MSYNC, 144, arm::MSYNC),
    (i386::READV, 145, arm::READV),
    (i386::WRITEV, 146, arm::WRITEV),
    (i386::GETSID, 147, arm::GETSID),
    (i386::FDATASYNC, 148, arm::FDATASYNC),
    (i386::SCHED_SETPARAM, 154, arm::SCHED_SETPARAM),
    (i386::SCHED_GETPARAM, 155, arm::SCHED_GETPARAM),
    (i386::SCHED_SETSCHEDULER, 156, arm::SCHED_SETSCHEDULER),
    (i386::SCHED_GETSCHEDULER, 157, arm::SCHED_GETSCHEDULER),
    (
        i386::SCHED_GET_PRIORITY_MAX,
        159,
        arm::SCHED_GET_PRIORITY_MAX,
    ),
    (
        i386::SCHED_GET_PRIORITY_MIN,
        160,
        arm::SCHED_GET_PRIORITY_MIN,
    ),
    (i386::MREMAP, 163, arm::MREMAP),
    (i386::POLL, 168, arm::POLL),
    (i386::PRCTL, 172, arm::PRCTL),
    (i386::PREAD64, 180, arm::PREAD64),
    (i386::PWRITE64, 181, arm::PWRITE64),
    (i386::GETCWD, 183, arm::GETCWD),
    (i386::CAPGET, 184, arm::CAPGET),
    (i386::CAPSET, 185, arm::CAPSET),
    (i386::MMAP2, 192, arm::MMAP2),
    (i386::TRUNCATE64, 193, arm::TRUNCATE64),
    (i386::FTRUNCATE64, 194, arm::FTRUNCATE64),
    (i386::LCHOWN32, 198, arm::LCHOWN32),
    (i386::SETREUID32, 203, arm::SETREUID32),
    (i386::SETREGID32, 204, arm::SETREGID32),
    (i386::GETGROUPS32, 205, arm::GETGROUPS32),
    (i386::SETGROUPS32, 206, arm::SETGROUPS32),
    (i386::FCHOWN32, 207, arm::FCHOWN32),
    (i386::SETRESUID32, 208, arm::SETRESUID32),
    (i386::GETRESUID32, 209, arm::GETRESUID32),
    (i386::SETRESGID32, 210, arm::SETRESGID32),
    (i386::GETRESGID32, 211, arm::GETRESGID32),
    (i386::CHOWN32, 212, arm::CHOWN32),
    (i386::SETUID32, 213, arm::SETUID32),
    (i386::SETGID32, 214, arm::SETGID32),
    (i386::SETFSUID32, 215, arm::SETFSUID32),
    (i386::SETFSGID32, 216, arm::SETFSGID32),
    (i386::PIVOT_ROOT, 217, arm::PIVOT_ROOT),
    (i386::MADVISE, 219, arm::MADVISE),
    (i386::GETDENTS64, 220, arm::GETDENTS64),
    (i386::FCNTL64, 221, arm::FCNTL64),
    (i386::READAHEAD, 225, arm::READAHEAD),
    (i386::SETXATTR, 226, arm::SETXATTR),
    (i386::LSETXATTR, 227, arm::LSETXATTR),
    (i386::FSETXATTR, 228, arm::FSETXATTR),
    (i386::GETXATTR, 229, arm::GETXATTR),
    (i386::LGETXATTR, 230, arm::LGETXATTR),
    (i386::FGETXATTR, 231, arm::FGETXATTR),
    (i386::LISTXATTR, 232, arm::LISTXATTR),
    (i386::LLISTXATTR, 233, arm::LLISTXATTR),
    (i386::FLISTXATTR, 234, arm::FLISTXATTR),
    (i386::REMOVEXATTR, 235, arm::REMOVEXATTR),
    (i386::LREMOVEXATTR, 236, arm::LREMOVEXATTR),
    (i386::FREMOVEXATTR, 237, arm::FREMOVEXATTR),
    (i386::SENDFILE64, 239, arm::SENDFILE64),
    (i386::SET_TID_ADDRESS, 258, arm::SET_TID_ADDRESS),
    (i386::EPOLL_CREATE, 254, arm::EPOLL_CREATE),
    (i386::EPOLL_CTL, 255, arm::EPOLL_CTL),
    (i386::EPOLL_WAIT, 256, arm::EPOLL_WAIT),
    (i386::INOTIFY_INIT, 291, arm::INOTIFY_INIT),
    (i386::INOTIFY_ADD_WATCH, 292, arm::INOTIFY_ADD_WATCH),
    (i386::INOTIFY_RM_WATCH, 293, arm::INOTIFY_RM_WATCH),
    (i386::OPENAT, 295, arm::OPENAT),
    (i386::MKDIRAT, 296, arm::MKDIRAT),
    (i386::MKNODAT, 297, arm::MKNODAT),
    (i386::FCHOWNAT, 298, arm::FCHOWNAT),
    (i386::UNLINKAT, 301, arm::UNLINKAT),
    (i386::RENAMEAT, 302, arm::RENAMEAT),
    (i386::LINKAT, 303, arm::LINKAT),
    (i386::SYMLINKAT, 304, arm::SYMLINKAT),
    (i386::READLINKAT, 305, arm::READLINKAT),
    (i386::FCHMODAT, 306, arm::FCHMODAT),
    (i386::FACCESSAT, 307, arm::FACCESSAT),
    (i386::UNSHARE, 310, arm::UNSHARE),
    (i386::SPLICE, 313, arm::SPLICE),
    (i386::GETCPU, 318, arm::GETCPU),
    (i386::EPOLL_PWAIT, 319, arm::EPOLL_PWAIT),
    (i386::SIGNALFD, 321, arm::SIGNALFD),
    (i386::TIMERFD_CREATE, 322, arm::TIMERFD_CREATE),
    (i386::EVENTFD, 323, arm::EVENTFD),
    (i386::FALLOCATE, 324, arm::FALLOCATE),
    (i386::SIGNALFD4, 327, arm::SIGNALFD4),
    (i386::EVENTFD2, 328, arm::EVENTFD2),
    (i386::EPOLL_CREATE1, 329, arm::EPOLL_CREATE1),
    (i386::PIPE2, 331, arm::PIPE2),
    (i386::INOTIFY_INIT1, 332, arm::INOTIFY_INIT1),
    (i386::PRLIMIT64, 340, arm::PRLIMIT64),
    (i386::NAME_TO_HANDLE_AT, 341, arm::NAME_TO_HANDLE_AT),
    (i386::SYNCFS, 344, arm::SYNCFS),
    (i386::SETNS, 346, arm::SETNS),
    (i386::RENAMEAT2, 353, arm::RENAMEAT2),
    (i386::GETRANDOM, 355, arm::GETRANDOM),
    (i386::SECCOMP, 354, arm::SECCOMP),
    (i386::MEMFD_CREATE, 356, arm::MEMFD_CREATE),
    (i386::EXECVEAT, 358, arm::EXECVEAT),
    (i386::MEMBARRIER, 375, arm::MEMBARRIER),
    (i386::COPY_FILE_RANGE, 377, arm::COPY_FILE_RANGE),
    (i386::STATX, 383, arm::STATX),
    (i386::CLOCK_GETTIME64, 403, arm::CLOCK_GETTIME64),
    (i386::CLOCK_SETTIME64, 404, arm::CLOCK_SETTIME64),
    (i386::CLOCK_GETRES_TIME64, 406, arm::CLOCK_GETRES_TIME64),
    (
        i386::CLOCK_NANOSLEEP_TIME64,
        407,
        arm::CLOCK_NANOSLEEP_TIME64,
    ),
    (i386::TIMERFD_GETTIME64, 410, arm::TIMERFD_GETTIME64),
    (i386::TIMERFD_SETTIME64, 411, arm::TIMERFD_SETTIME64),
    (i386::UTIMENSAT_TIME64, 412, arm::UTIMENSAT_TIME64),
    (i386::PPOLL_TIME64, 414, arm::PPOLL_TIME64),
    (i386::FUTEX_TIME64, 422, arm::FUTEX_TIME64),
    (i386::SEMGET, 393, arm::SEMGET),
    (i386::SEMCTL, 394, arm::SEMCTL),
    (i386::SEMTIMEDOP_TIME64, 420, arm::SEMTIMEDOP_TIME64),
    (i386::RT_SIGPENDING, 176, arm::RT_SIGPENDING),
    (i386::PAUSE, 29, arm::PAUSE),
    (i386::PIDFD_OPEN, 434, arm::PIDFD_OPEN),
    (i386::OPENAT2, 437, arm::OPENAT2),
    (i386::FACCESSAT2, 439, arm::FACCESSAT2),
    (i386::EPOLL_PWAIT2, 441, arm::EPOLL_PWAIT2),
    (i386::STATFS64, 268, arm::STATFS64),
    (i386::FSTATFS64, 269, arm::FSTATFS64),
    (i386::SYSINFO, 116, arm::SYSINFO),
    (i386::REBOOT, 88, arm::REBOOT),
    (i386::SYSLOG, 103, arm::SYSLOG),
    (i386::SCHED_SETAFFINITY, 241, arm::SCHED_SETAFFINITY),
    (i386::SCHED_GETAFFINITY, 242, arm::SCHED_GETAFFINITY),
    (i386::PSELECT6_TIME64, 413, arm::PSELECT6_TIME64),
    (i386::TIMES, 43, arm::TIMES),
    (i386::GETRUSAGE, 77, arm::GETRUSAGE),
    (i386::NANOSLEEP, 162, arm::NANOSLEEP),
    (i386::CLOCK_NANOSLEEP, 267, arm::CLOCK_NANOSLEEP),
    (i386::FUTEX, 240, arm::FUTEX),
    (i386::CLOCK_GETTIME, 265, arm::CLOCK_GETTIME),
    (i386::CLOCK_GETRES, 266, arm::CLOCK_GETRES),
    (i386::CLOCK_SETTIME, 264, arm::CLOCK_SETTIME),
    (i386::PPOLL, 309, arm::PPOLL),
    (i386::PSELECT6, 308, arm::PSELECT6),
    (i386::NEWSELECT, 142, arm::NEWSELECT),
    (i386::UTIMENSAT, 320, arm::UTIMENSAT),
    (i386::TIMERFD_SETTIME, 325, arm::TIMERFD_SETTIME),
    (i386::TIMERFD_GETTIME, 326, arm::TIMERFD_GETTIME),
    (i386::GETTIMEOFDAY, 78, arm::GETTIMEOFDAY),
    (i386::SETTIMEOFDAY, 79, arm::SETTIMEOFDAY),
    (i386::ADJTIMEX, 124, arm::ADJTIMEX),
    (i386::CLOCK_ADJTIME, 343, arm::CLOCK_ADJTIME),
    (i386::CLOCK_ADJTIME64, 405, arm::CLOCK_ADJTIME64),
    (i386::SOCKET, 359, arm::SOCKET),
    (i386::SOCKETPAIR, 360, arm::SOCKETPAIR),
    (i386::BIND, 361, arm::BIND),
    (i386::CONNECT, 362, arm::CONNECT),
    (i386::LISTEN, 363, arm::LISTEN),
    (i386::ACCEPT4, 364, arm::ACCEPT4),
    (i386::GETSOCKOPT, 365, arm::GETSOCKOPT),
    (i386::SETSOCKOPT, 366, arm::SETSOCKOPT),
    (i386::GETSOCKNAME, 367, arm::GETSOCKNAME),
    (i386::GETPEERNAME, 368, arm::GETPEERNAME),
    (i386::SENDTO, 369, arm::SENDTO),
    (i386::SENDMSG, 370, arm::SENDMSG),
    (i386::RECVFROM, 371, arm::RECVFROM),
    (i386::RECVMSG, 372, arm::RECVMSG),
    (i386::SHUTDOWN, 373, arm::SHUTDOWN),
    (i386::UGETRLIMIT, 191, arm::UGETRLIMIT),
    (i386::SETRLIMIT, 75, arm::SETRLIMIT),
    (i386::SET_ROBUST_LIST, 311, arm::SET_ROBUST_LIST),
    (i386::GET_ROBUST_LIST, 312, arm::GET_ROBUST_LIST),
    (i386::STAT64, 195, arm::STAT64),
    (i386::LSTAT64, 196, arm::LSTAT64),
    (i386::FSTAT64, 197, arm::FSTAT64),
    (i386::FSTATAT64, 300, arm::FSTATAT64),
    (i386::RSEQ, 386, arm::RSEQ),
    (i386::WAITID, 284, arm::WAITID),
];

/// Every i386 number [`from_i386`] translates, as `asm/unistd_32.h` numbers
/// it and as ARMv7-A's table -- the other 32-bit one, whose `32` credential
/// forms and `exit_group` these share -- names the call. A number copied
/// from the wrong table, or onto the wrong call, fails here.
#[test]
fn the_i386_numbers_are_the_headers_and_mean_what_arm_means() {
    for (constant, header, on_arm) in I386_NUMBERS {
        assert_eq!(constant, header, "i386 number {constant}");
        assert!(from_i386(constant).is_some(), "{constant} translates");
        assert_eq!(from_i386(constant), from_arm(on_arm), "i386 {constant}");
    }
    // The two thread-area calls exist only here, so ARMv7-A has nothing to
    // compare them with; their numbers are the header's.
    assert_eq!((i386::SET_THREAD_AREA, i386::GET_THREAD_AREA), (243, 244));
    assert_eq!(from_i386(243), Some(Syscall::SetThreadArea));
    assert_eq!(from_i386(244), Some(Syscall::GetThreadArea));
    // `alarm` and `time`, which ARM's EABI dropped and i386 kept.
    assert_eq!((i386::ALARM, i386::TIME), (27, 13));
    assert_eq!(from_i386(27), Some(Syscall::Alarm));
    assert_eq!(from_i386(13), Some(Syscall::Time));
    // `socketcall`, which only i386 has among these tables.
    assert_eq!(i386::SOCKETCALL, 102);
    assert_eq!(from_i386(102), Some(Syscall::Socketcall));
    // `ipc`, the same for System V IPC, against `asm-x86/unistd_32.h`.
    assert_eq!(i386::IPC, 117);
    assert_eq!(from_i386(117), Some(Syscall::Ipc));
    let translated = (0..I386_END).filter(|&nr| from_i386(nr).is_some()).count();
    assert_eq!(
        translated,
        I386_NUMBERS.len() + 6,
        "no i386 number is mapped unread"
    );
    // The numbers x86-64 gives these calls are other calls, or none, on
    // i386: `int $0x80` from a 64-bit program is an i386 call, so the two
    // tables must never be confused.
    assert_eq!(
        from_i386(x86_64::WRITE),
        Some(Syscall::Exit),
        "x86-64's write is i386's exit"
    );
    assert_eq!(
        from_i386(x86_64::GETPID),
        Some(Syscall::Mkdir),
        "x86-64's getpid is i386's mkdir"
    );
    // i386's `mmap` at 90 takes a pointer to its six arguments: nothing
    // reads that block yet, so the number stays unmapped rather than being
    // read as `mmap2`'s registers. The boot's `HELLO_I386` also asks 90 for
    // its `-ENOSYS`; mapping it means moving that probe too.
    assert_eq!(from_i386(90), None, "old_mmap is not mmap2");
}

// -- struct user_desc ---------------------------------------------------------

/// musl's own request, from `src/thread/i386/__set_thread_area.s`: entry -1,
/// its thread block as the base, limit 0xfffff, flags 0x51. The descriptor
/// is `fill_ldt`'s, worked by hand: access 0xf3 (present, ring 3, data,
/// writable, accessed), flags 0xd (page-granular, 32-bit, available).
#[test]
fn musls_thread_segment_encodes_as_linux_encodes_it() {
    use crate::user_desc::{ANY_ENTRY, UserDesc};
    let mut bytes = [0_u8; 16];
    bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes[4..8].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&0x000f_ffff_u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&0x51_u32.to_le_bytes());
    let desc = UserDesc::from_bytes(&bytes).unwrap();
    assert_eq!(desc.entry_number, ANY_ENTRY);
    assert!(desc.seg_32bit && desc.limit_in_pages && desc.useable);
    assert!(!desc.read_exec_only && !desc.seg_not_present && desc.contents == 0);
    assert!(!desc.clears());
    let descriptor = desc.to_descriptor().unwrap();
    assert_eq!(descriptor, 0x12df_f334_5678_ffff);
    // And back, as get_thread_area reads it.
    let read = UserDesc::from_descriptor(12, descriptor);
    assert_eq!(
        read,
        UserDesc {
            entry_number: 12,
            ..desc
        }
    );
    let mut expected = bytes;
    expected[0..4].copy_from_slice(&12_u32.to_le_bytes());
    assert_eq!(read.to_bytes(), expected);
    // `lm`, bit 7, is a 32-bit program's garbage and changes nothing.
    bytes[12] |= 0x80;
    assert_eq!(UserDesc::from_bytes(&bytes).unwrap(), desc);
}

/// `tls_desc_okay`'s refusals, its two ways of emptying an entry, and an
/// empty entry reading back in the shape that empties one.
#[test]
fn thread_area_descriptors_are_refused_and_cleared_as_linux_does() {
    use crate::user_desc::{Refused, UserDesc, tls_index};
    let data = UserDesc {
        entry_number: 13,
        base_addr: 0x4000,
        limit: 0xfffff,
        seg_32bit: true,
        contents: 0,
        read_exec_only: false,
        limit_in_pages: true,
        seg_not_present: false,
        useable: false,
    };
    assert!(data.to_descriptor().is_ok());
    for refused in [
        UserDesc {
            seg_32bit: false,
            ..data
        },
        UserDesc {
            contents: 2,
            ..data
        },
        UserDesc {
            contents: 3,
            ..data
        },
        UserDesc {
            seg_not_present: true,
            ..data
        },
    ] {
        assert_eq!(refused.to_descriptor(), Err(Refused), "{refused:?}");
    }
    // Expand-down data is data, and allowed.
    assert!(
        UserDesc {
            contents: 1,
            ..data
        }
        .to_descriptor()
        .is_ok()
    );

    let zero = UserDesc::from_bytes(&[0; 16]).unwrap();
    assert!(zero.clears());
    assert_eq!(zero.to_descriptor(), Ok(0));
    let empty = UserDesc::from_descriptor(14, 0);
    assert!(empty.read_exec_only && empty.seg_not_present && !empty.seg_32bit);
    assert!(
        empty.clears(),
        "an emptied entry reads back as a request to empty one"
    );

    assert_eq!(UserDesc::from_bytes(&[0; 15]), None);
    assert_eq!(
        (tls_index(11), tls_index(12), tls_index(14), tls_index(15)),
        (None, Some(0), Some(2), None)
    );
    assert_eq!(tls_index(u32::MAX), None);
}

/// Every descriptor `user_desc` can build is one ring 3 may hold and nothing
/// more (certification review, T.ESCALATE path 7): walked over every flag
/// word -- `lm`, bit 7, included -- with bases and limits at their ends.
/// Accepted means present, a code-or-data segment (S=1), DPL 3, not 64-bit,
/// and data, never code or conforming code; anything else is refused, or
/// empties the entry to the null descriptor. There is no field for a
/// privilege level, a system type or a gate, so none can be asked for.
#[test]
fn a_user_desc_can_only_ever_describe_ring_3_data() {
    use crate::user_desc::UserDesc;
    const PRESENT: u64 = 1 << 47;
    const CODE_OR_DATA: u64 = 1 << 44;
    const DPL_3: u64 = 3 << 45;
    const EXECUTABLE: u64 = 1 << 43;
    const LONG_MODE: u64 = 1 << 53;
    let mut accepted = 0;
    for flags in 0_u32..256 {
        for (base, limit) in [(0, 0), (0xffff_ffff, 0xf_ffff), (0x1234_5678, 0x1_0000)] {
            let mut bytes = [0_u8; 16];
            bytes[4..8].copy_from_slice(&u32::to_le_bytes(base));
            bytes[8..12].copy_from_slice(&u32::to_le_bytes(limit));
            bytes[12..16].copy_from_slice(&flags.to_le_bytes());
            let desc = UserDesc::from_bytes(&bytes).unwrap();
            match desc.to_descriptor() {
                Ok(0) => assert!(desc.clears(), "{flags:#x}: null without clearing"),
                Ok(descriptor) => {
                    accepted += 1;
                    assert_eq!(descriptor & PRESENT, PRESENT, "{flags:#x}");
                    assert_eq!(descriptor & CODE_OR_DATA, CODE_OR_DATA, "{flags:#x}");
                    assert_eq!(descriptor & DPL_3, DPL_3, "{flags:#x}");
                    assert_eq!(descriptor & (EXECUTABLE | LONG_MODE), 0, "{flags:#x}");
                }
                Err(_) => {
                    let contents = (flags >> 1) & 3;
                    assert!(
                        flags & 1 == 0 || contents > 1 || flags & (1 << 5) != 0,
                        "{flags:#x} was refused though tls_desc_okay takes it"
                    );
                }
            }
        }
    }
    // 32-bit, data or expand-down, present: four free bits (read-only,
    // pages, useable, lm), two contents, three shapes of base and limit.
    assert_eq!(accepted, 16 * 2 * 3);
}

// -- i386 signal frames --------------------------------------------------------

/// Every offset of the i386 frames is the sum of its declaration's fields in
/// the Linux 7.0 headers (`sigframe.h`, `ia32.h`, `sigcontext.h`, `compat.h`);
/// these are the totals worked by hand from them.
#[test]
fn the_i386_frames_are_linuxs_sizes() {
    use crate::sigframe32::{
        FPSTATE_SIZE, FXSAVE_AT, frame, rt_frame, sigcontext, stack, ucontext,
    };
    assert_eq!(sigcontext::SIZE, 22 * 4, "sigcontext_32 is 22 words");
    assert_eq!(stack::SIZE, 12);
    assert_eq!(
        (ucontext::MCONTEXT, ucontext::SIGMASK, ucontext::SIZE),
        (20, 108, 116)
    );
    assert_eq!(
        (
            rt_frame::INFO,
            rt_frame::UC,
            rt_frame::RETCODE,
            rt_frame::SIZE
        ),
        (16, 144, 260, 268)
    );
    assert_eq!(
        FXSAVE_AT,
        7 * 4 + 8 * 10 + 2 + 2,
        "the fsave environment, registers, status, magic"
    );
    assert_eq!(FPSTATE_SIZE, 624);
    assert_eq!(
        (frame::SC, frame::EXTRAMASK, frame::RETCODE, frame::SIZE),
        (8, 720, 724, 732)
    );
}

/// The union moves down four bytes and nothing else changes: a sender's pid
/// and uid, a child's status, and a fault's address below 4 GiB land where a
/// 32-bit program's `siginfo_t` has them (union at 12, `si_status` at 20).
#[test]
fn a_siginfo_is_read_by_a_32_bit_program_where_it_looks() {
    use crate::sigframe32::siginfo_from_64;
    let mut info = [0_u8; 128];
    info[0..4].copy_from_slice(&17_i32.to_le_bytes()); // SIGCHLD
    info[8..12].copy_from_slice(&1_i32.to_le_bytes()); // CLD_EXITED
    info[16..20].copy_from_slice(&4242_i32.to_le_bytes()); // si_pid
    info[20..24].copy_from_slice(&1000_i32.to_le_bytes()); // si_uid
    info[24..28].copy_from_slice(&7_i32.to_le_bytes()); // si_status
    let out = siginfo_from_64(&info);
    assert_eq!(out[0..12], info[0..12]);
    assert_eq!(out[12..16], 4242_i32.to_le_bytes());
    assert_eq!(out[16..20], 1000_i32.to_le_bytes());
    assert_eq!(out[20..24], 7_i32.to_le_bytes());
    assert!(out[124..].iter().all(|&byte| byte == 0));

    let mut fault = [0_u8; 128];
    fault[16..24].copy_from_slice(&0x0804_9000_u64.to_le_bytes());
    assert_eq!(
        siginfo_from_64(&fault)[12..16],
        0x0804_9000_u32.to_le_bytes()
    );
}

/// `compat_sigaction`: handler, flags, restorer, then the mask's two words.
#[test]
fn a_32_bit_sigaction_round_trips() {
    use crate::sigframe32::Sigaction32;
    let action = Sigaction32 {
        handler: 0x0804_8123,
        flags: 0x0400_0004, // SA_RESTORER | SA_SIGINFO
        restorer: 0x0804_8456,
        mask: 0x8000_0000_0000_0001,
    };
    let bytes = action.to_bytes();
    assert_eq!(bytes[12..16], 1_u32.to_le_bytes());
    assert_eq!(bytes[16..20], 0x8000_0000_u32.to_le_bytes());
    assert_eq!(Sigaction32::from_bytes(&bytes), Some(action));
    assert_eq!(Sigaction32::from_bytes(&bytes[..19]), None);
}

/// i386's `struct stat64`: 96 bytes with `st_size` at 44, four bytes earlier
/// than ARMv7-A's, because the i386 ABI aligns a `long long` to four. Offsets
/// from `arch/x86/include/uapi/asm/stat.h` laid out by that rule, and the
/// size Linux's packed compat `stat64` has.
#[test]
fn i386s_stat64_is_packed_to_four() {
    use crate::types::i386::Stat64;
    use core::mem::{offset_of, size_of};
    assert_eq!(size_of::<Stat64>(), 96, "i386 struct stat64 is 96 bytes");
    let at = [
        (offset_of!(Stat64, __st_ino), 12),
        (offset_of!(Stat64, st_mode), 16),
        (offset_of!(Stat64, st_rdev), 32),
        (offset_of!(Stat64, st_size), 44),
        (offset_of!(Stat64, st_blksize), 52),
        (offset_of!(Stat64, st_blocks), 56),
        (offset_of!(Stat64, st_atime), 64),
        (offset_of!(Stat64, st_ctime_nsec), 84),
        (offset_of!(Stat64, st_ino), 88),
    ];
    for (offset, want) in at {
        assert_eq!(offset, want, "a field of i386's struct stat64");
    }
}

#[test]
fn block_requests_match_linux_fs_h() {
    // `include/uapi/linux/fs.h` of Linux 6.18, as `_IO(0x12, n)`.
    let io = |nr: u32| (0x12 << 8) | nr;
    assert_eq!(types::BLKROGET, io(94));
    assert_eq!(types::BLKRRPART, io(95));
    assert_eq!(types::BLKGETSIZE, io(96));
    assert_eq!(types::BLKFLSBUF, io(97));
    assert_eq!(types::BLKSSZGET, io(104));
    assert_eq!(types::BLKIOMIN, io(120));
    assert_eq!(types::BLKIOOPT, io(121));
    assert_eq!(types::BLKALIGNOFF, io(122));
    assert_eq!(types::BLKPBSZGET, io(123));
    assert_eq!(types::BLKROTATIONAL, io(126));
    // `_IOR(0x12, n, size_t)`: what a 64-bit `blockdev --getsize64` passes.
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(types::BLKBSZGET, 0x8008_1270);
        assert_eq!(types::BLKGETSIZE64, 0x8008_1272);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert_eq!(types::BLKBSZGET, 0x8004_1270);
        assert_eq!(types::BLKGETSIZE64, 0x8004_1272);
    }
}

#[test]
fn seccomp_is_where_the_headers_put_it() {
    // `__NR_seccomp` in asm-x86/unistd_64.h, unistd_32.h, asm-generic/unistd.h
    // and asm-arm/unistd-common.h: the generic table AArch64 uses is 277, and
    // x86-64's 317, i386's 354 and EABI's 383 are each their own.
    for (number, from, call) in [
        (317, from_x86_64 as fn(usize) -> Option<Syscall>, "x86-64"),
        (277, from_aarch64, "AArch64"),
        (383, from_arm, "ARMv7-A"),
        (354, from_i386, "i386"),
    ] {
        assert_eq!(
            from(number),
            Some(Syscall::Seccomp),
            "{call}: seccomp is {number}"
        );
    }
    assert_eq!(x86_64::SECCOMP, 317);
    assert_eq!(aarch64::SECCOMP, 277);
    assert_eq!(arm::SECCOMP, 383);
    assert_eq!(i386::SECCOMP, 354);
}
