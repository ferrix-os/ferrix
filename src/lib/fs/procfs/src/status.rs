//! `/proc/<pid>/status`, and the CPU mask and list formats.
//!
//! ```text
//! Name:\tsh
//! State:\tR (running)
//! Uid:\t0\t0\t0\t0
//! VmSize:\t    8252 kB
//! Cpus_allowed:\tf
//! Cpus_allowed_list:\t0-3
//! ```
//!
//! A tab after every colon, which `fs/proc/array.c` writes literally, and the
//! memory lines right-aligned in eight like `/proc/meminfo`'s. Readers look a
//! line up by its label, so [`Status`] carries only the lines a kernel can
//! fill truthfully, in Linux's order; a line Linux prints and this does not is
//! a line the kernel has nothing honest to put in yet.

use alloc::vec::Vec;

use crate::text::put;

/// What a process is doing, as `task_state_array` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// `R`: running or runnable.
    Running,
    /// `S`: waiting for something, interruptibly.
    Sleeping,
    /// `D`: waiting uninterruptibly, usually on I/O.
    DiskSleep,
    /// `T`: stopped by a signal.
    Stopped,
    /// `t`: stopped by a tracer.
    TracingStop,
    /// `X`: being reaped.
    Dead,
    /// `Z`: exited and not yet waited for.
    Zombie,
    /// `P`: a parked kernel thread.
    Parked,
    /// `I`: an idle kernel thread.
    Idle,
}

impl State {
    /// The letter `/proc/<pid>/stat` prints.
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            State::Running => 'R',
            State::Sleeping => 'S',
            State::DiskSleep => 'D',
            State::Stopped => 'T',
            State::TracingStop => 't',
            State::Dead => 'X',
            State::Zombie => 'Z',
            State::Parked => 'P',
            State::Idle => 'I',
        }
    }

    /// The letter and word `/proc/<pid>/status` prints.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            State::Running => "R (running)",
            State::Sleeping => "S (sleeping)",
            State::DiskSleep => "D (disk sleep)",
            State::Stopped => "T (stopped)",
            State::TracingStop => "t (tracing stop)",
            State::Dead => "X (dead)",
            State::Zombie => "Z (zombie)",
            State::Parked => "P (parked)",
            State::Idle => "I (idle)",
        }
    }
}

/// What the kernel can say about a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status<'a> {
    /// `Name`: the command name, at most fifteen bytes.
    pub name: &'a [u8],
    /// `Umask`.
    pub umask: u32,
    /// `State`.
    pub state: State,
    /// `Tgid` and `NStgid`: the process's id.
    pub tgid: u32,
    /// `Pid` and `NSpid`: the thread's id, which is the process's for
    /// `/proc/<pid>/status` and for its main thread's `task/<pid>/status`.
    pub pid: u32,
    /// `PPid`.
    pub ppid: u32,
    /// `Uid`: the real, effective, saved and filesystem user ids.
    pub uid: [u32; 4],
    /// `Gid`: the group ids, in the same order.
    pub gid: [u32; 4],
    /// `FDSize`: slots in the descriptor table.
    pub fd_size: u32,
    /// `VmSize`: every mapped region, in kibibytes.
    pub vm_size: u64,
    /// `VmLck`: the `mlock`ed part.
    pub vm_locked: u64,
    /// `VmData`: private writable memory that is not the stack.
    pub vm_data: u64,
    /// `VmStk`: the main thread's stack.
    pub vm_stack: u64,
    /// `Threads`.
    pub threads: u32,
    /// `NoNewPrivs`.
    pub no_new_privs: bool,
    /// `Seccomp`: the thread's mode, 0 (none), 1 (strict) or 2 (filter).
    pub seccomp: u32,
    /// `Seccomp_filters`: how many filters the thread's chain holds.
    pub seccomp_filters: u32,
    /// Processors, all of which the process may run on.
    pub cpus: u32,
}

/// Append the whole file.
pub fn render(out: &mut Vec<u8>, status: &Status<'_>) {
    out.extend_from_slice(b"Name:\t");
    escape_name(out, status.name);
    let Status {
        tgid,
        pid,
        uid,
        gid,
        ..
    } = *status;
    let [ruid, euid, suid, fsuid] = uid;
    let [rgid, egid, sgid, fsgid] = gid;
    put(
        out,
        format_args!(
            "\nUmask:\t{:04o}\nState:\t{}\nTgid:\t{tgid}\nNgid:\t0\nPid:\t{pid}\nPPid:\t{}\n\
             TracerPid:\t0\nUid:\t{ruid}\t{euid}\t{suid}\t{fsuid}\n\
             Gid:\t{rgid}\t{egid}\t{sgid}\t{fsgid}\n\
             FDSize:\t{}\nGroups:\t \nNStgid:\t{tgid}\nNSpid:\t{pid}\n",
            status.umask,
            status.state.description(),
            status.ppid,
            status.fd_size,
        ),
    );
    for (label, kib) in [
        ("VmSize", status.vm_size),
        ("VmLck", status.vm_locked),
        ("VmData", status.vm_data),
        ("VmStk", status.vm_stack),
    ] {
        put(out, format_args!("{label}:\t{kib:>8} kB\n"));
    }
    // `NoNewPrivs`, `Seccomp` and `Seccomp_filters` come after the capability
    // sets and before the speculation lines and `Cpus_allowed`
    // (`fs/proc/array.c`), which is where the lines Ferrix has fall.
    put(
        out,
        format_args!(
            "Threads:\t{}\nNoNewPrivs:\t{}\nSeccomp:\t{}\nSeccomp_filters:\t{}\nCpus_allowed:\t",
            status.threads,
            u32::from(status.no_new_privs),
            status.seccomp,
            status.seccomp_filters,
        ),
    );
    cpu_mask(out, status.cpus);
    out.extend_from_slice(b"\nCpus_allowed_list:\t");
    cpu_list(out, status.cpus);
    out.push(b'\n');
}

/// The name as `proc_task_name` escapes it for `status`: a newline or a
/// backslash would break the line-per-field format, so both are escaped the
/// way C writes them.
fn escape_name(out: &mut Vec<u8>, name: &[u8]) {
    for &byte in name {
        match byte {
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            _ => out.push(byte),
        }
    }
}

/// `%*pb` of a mask with the first `cpus` of `cpus` bits set.
///
/// Hexadecimal in 32-bit chunks from the most significant, separated by
/// commas; the first chunk has as many digits as its bits need and every
/// other has eight. Four processors are `f`, and 36 are `f,ffffffff`.
pub fn cpu_mask(out: &mut Vec<u8>, cpus: u32) {
    let bits = cpus.max(1);
    let mut chunk_bits = match bits % 32 {
        0 => 32,
        partial => partial,
    };
    let mut remaining = bits;
    let mut first = true;
    while remaining > 0 {
        if !first {
            out.push(b',');
        }
        first = false;
        let value = if chunk_bits == 32 {
            u32::MAX
        } else {
            (1_u32 << chunk_bits) - 1
        };
        let digits = chunk_bits.div_ceil(4) as usize;
        put(out, format_args!("{value:0digits$x}"));
        remaining -= chunk_bits;
        chunk_bits = 32;
    }
}

/// `%*pbl` of the same mask: `0-3`, or `0` for one processor.
pub fn cpu_list(out: &mut Vec<u8>, cpus: u32) {
    match cpus {
        0 | 1 => out.push(b'0'),
        _ => put(out, format_args!("0-{}", cpus - 1)),
    }
}
