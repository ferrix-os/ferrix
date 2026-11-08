//! DIAG, never to land (fault-relanding): why a program's first thread leaves
//! while its other threads live on. Endfield's leader went zombie with 143
//! threads live about 5 s after it forked its embedded browser; this logs
//! every way that can happen, each line prefixed `fxdiag:`.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_vfs::OpenFile;

use crate::fs;
use crate::syscall::process::Process;
use crate::syscall::uaccess;

/// Lines left to print, so that nothing here floods the log.
static BUDGET: AtomicU32 = AtomicU32::new(1200);
/// Exit lines left.
static EXITS: AtomicU32 = AtomicU32::new(100);
/// Fault lines left.
static FAULTS: AtomicU32 = AtomicU32::new(300);
/// `EFAULT` lines left.
static EFAULTS: AtomicU32 = AtomicU32::new(200);
/// Signal lines left.
static SIGNALS: AtomicU32 = AtomicU32::new(300);

fn take(budget: &AtomicU32) -> bool {
    budget
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(1))
        .is_ok()
        && BUDGET
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(1))
            .is_ok()
}

/// The last calls of watched processes' first threads: pid and number,
/// answer, three arguments, time.
const RING: usize = 512;
static CALLS: [[core::sync::atomic::AtomicU64; 6]; RING] =
    [const { [const { core::sync::atomic::AtomicU64::new(0) }; 6] }; RING];
static NEXT: AtomicU32 = AtomicU32::new(0);

/// Record a call of a watched process's first thread.
pub(crate) fn record(process: &Process, tid: u32, number: usize, answer: u64, args: &[u64; 6]) {
    if tid != process.pid() || !watched(process) {
        return;
    }
    let slot = NEXT.fetch_add(1, Ordering::Relaxed) as usize % RING;
    let entry = &CALLS[slot];
    entry[0].store(u64::from(tid) << 32 | number as u64, Ordering::Relaxed);
    entry[1].store(answer, Ordering::Relaxed);
    entry[2].store(args[0], Ordering::Relaxed);
    entry[3].store(args[1], Ordering::Relaxed);
    entry[4].store(args[2], Ordering::Relaxed);
    entry[5].store(crate::timer::now_nanos(), Ordering::Relaxed);
}

/// Print `tid`'s last recorded calls, oldest first.
fn print_calls(tid: u32) {
    let end = NEXT.load(Ordering::Relaxed) as usize;
    let mut shown = 0;
    for back in (1..=RING).rev() {
        let Some(at) = end.checked_sub(back) else {
            continue;
        };
        let entry = &CALLS[at % RING];
        let head = entry[0].load(Ordering::Relaxed);
        if (head >> 32) as u32 != tid {
            continue;
        }
        shown += 1;
        if shown > 80 {
            break;
        }
        crate::println!(
            "fxdiag:   call {} -> {:#x} ({:#x}, {:#x}, {:#x}) at {} us",
            head & 0xffff_ffff,
            entry[1].load(Ordering::Relaxed),
            entry[2].load(Ordering::Relaxed),
            entry[3].load(Ordering::Relaxed),
            entry[4].load(Ordering::Relaxed),
            entry[5].load(Ordering::Relaxed) / 1000
        );
    }
}

/// A process big enough to be the game: more threads than any service has.
pub(crate) fn watched(process: &Process) -> bool {
    process.live_thread_count() >= 40
}

fn name_of(process: &Process) -> Vec<u8> {
    process.comm()
}

fn thread_name() -> [u8; 16] {
    let mut out = [b' '; 16];
    if let Some(thread) = crate::syscall::thread::current()
        && let Some(name) = thread.name()
    {
        for (o, b) in out.iter_mut().zip(name.iter()) {
            *o = if *b == 0 { b' ' } else { *b };
        }
    }
    out
}

/// Print where `address` lies: the file and its offset, or the anonymous
/// region's start and the rights.
fn print_where(process: &Process, label: &str, address: u64) {
    let Ok(regions) = process.space().regions() else {
        crate::println!("fxdiag:   {} {:#x} (no region list)", label, address);
        return;
    };
    let Some(region) = regions
        .iter()
        .find(|region| region.start <= address && address < region.end)
    else {
        crate::println!("fxdiag:   {} {:#x} unmapped", label, address);
        return;
    };
    let rights = [
        if region.flags.read { b'r' } else { b'-' },
        if region.flags.write { b'w' } else { b'-' },
        if region.flags.execute { b'x' } else { b'-' },
        if region.flags.shared { b's' } else { b'p' },
    ];
    let rights = core::str::from_utf8(&rights).unwrap_or("????");
    let file = region.file.and_then(|(id, offset)| {
        let file = process
            .space()
            .mapped_file(id)?
            .downcast::<OpenFile>()
            .ok()?;
        Some((file, offset))
    });
    match file {
        Some((file, offset)) => {
            let root = process.fs_context().lock().root.clone();
            let path = fs::namespace().path_of(file.location(), &root);
            crate::println!(
                "fxdiag:   {} {:#x} {} {}+{:#x}",
                label,
                address,
                rights,
                core::str::from_utf8(&path).unwrap_or("?"),
                address - region.start + offset
            );
        }
        None => crate::println!(
            "fxdiag:   {} {:#x} {} anon {:#x}-{:#x}+{:#x}",
            label,
            address,
            rights,
            region.start,
            region.end,
            address - region.start
        ),
    }
}

/// Print the user stack from `sp`: every word that points into executable
/// memory, which is about what a backtrace would show.
fn print_stack(process: &Process, from: u64, sp: u64) {
    let mut words = alloc::vec![0_u8; if from < sp { (sp - from) as usize } else { 8 * 512 }];
    let mut got = 0;
    // A page at a time, so that a stack near its end still gives its top.
    while got < words.len() {
        let page_left = 4096 - ((from + got as u64) & 4095) as usize;
        let n = page_left.min(words.len() - got);
        if uaccess::copy_from_user(process.space(), from + got as u64, &mut words[got..got + n])
            .is_err()
        {
            if from < sp {
                // Below the stack's lowest page: skip on.
                got += n;
                continue;
            }
            break;
        }
        got += n;
    }
    let Ok(regions) = process.space().regions() else {
        return;
    };
    let mut shown = 0;
    for (i, chunk) in words[..got - got % 8].chunks_exact(8).enumerate() {
        let mut word = [0_u8; 8];
        word.copy_from_slice(chunk);
        let value = u64::from_le_bytes(word);
        if regions
            .iter()
            .any(|region| region.flags.execute && region.start <= value && value < region.end)
        {
            let at = from + (i * 8) as u64;
            let mut label = [0_u8; 12];
            let label = fmt_label(&mut label, at.abs_diff(sp) as usize, at < sp);
            print_where(process, label, value);
            shown += 1;
            if shown >= 120 {
                break;
            }
        }
    }
}

fn fmt_label(out: &mut [u8; 12], offset: usize, below: bool) -> &str {
    let mut n = offset;
    let mut digits = [0_u8; 8];
    let mut len = 0;
    loop {
        digits[len] = b"0123456789abcdef"[n % 16];
        n /= 16;
        len += 1;
        if n == 0 || len == digits.len() {
            break;
        }
    }
    out[0] = b's';
    out[1] = b'p';
    out[2] = if below { b'-' } else { b'+' };
    for i in 0..len {
        out[3 + i] = digits[len - 1 - i];
    }
    core::str::from_utf8(&out[..3 + len]).unwrap_or("sp+?")
}

/// `exit` or `exit_group` from a watched process, or `exit` from any
/// process's first thread while others live.
pub(crate) fn exit_call(process: &Process, tid: u32, group: bool, status: i32, ip: u64, sp: u64) {
    let leader_alone = !group && tid == process.pid() && process.live_thread_count() > 1;
    if !leader_alone && !(group && watched(process)) {
        return;
    }
    if !take(&EXITS) {
        return;
    }
    let name = name_of(process);
    crate::println!(
        "fxdiag: {} pid {} tid {} ({}/{}) status {} threads {} ip {:#x} sp {:#x}",
        if group { "exit_group" } else { "LEADER exit" },
        process.pid(),
        tid,
        core::str::from_utf8(&name).unwrap_or("?"),
        core::str::from_utf8(&thread_name()).unwrap_or("?"),
        status,
        process.live_thread_count(),
        ip,
        sp
    );
    print_where(process, "ip", ip);
    print_calls(tid);
    print_stack(process, sp.saturating_sub(32 * 1024), sp);
    print_stack(process, sp, sp);
}

/// The end of a thread, whatever ended it: the first thread of a process
/// whose other threads live on.
pub(crate) fn thread_end(process: &Process, tid: u32, status: Option<i32>, group: bool) {
    if group || tid != process.pid() || process.live_thread_count() <= 1 {
        return;
    }
    if !take(&EXITS) {
        return;
    }
    crate::println!(
        "fxdiag: LEADER ends pid {} tid {} status {:?} threads {} terminated {} exec_thread {}",
        process.pid(),
        tid,
        status,
        process.live_thread_count(),
        process.is_terminated(),
        process.exec_thread_now()
    );
    if status.is_none() {
        print_calls(tid);
    }
}

/// A signal sent to one thread of a watched process, or to any process's
/// first thread, that may end it.
pub(crate) fn signal_to_thread(process: &Process, tid: u32, signal: u32, origin: &dyn core::fmt::Debug) {
    // SIGUSR1/2 (Wine's suspend and context calls), SIGCHLD, SIGALRM, SIGWINCH,
    // SIGURG, SIGIO and the real-time ones are routine.
    if matches!(signal, 10 | 12 | 14 | 17 | 23 | 28 | 29) || signal >= 32 {
        return;
    }
    if !(tid == process.pid() || watched(process)) {
        return;
    }
    if !take(&SIGNALS) {
        return;
    }
    crate::println!(
        "fxdiag: signal {} to pid {} tid {} ({}) from {:?} by pid {} tid {}",
        signal,
        process.pid(),
        tid,
        core::str::from_utf8(&name_of(process)).unwrap_or("?"),
        origin,
        crate::syscall::process::current().map_or(0, |p| p.pid()),
        crate::syscall::thread::current().map_or(0, |t| t.tid())
    );
}

/// A signal sent to a whole watched process.
pub(crate) fn signal_to_process(process: &Process, signal: u32, origin: &dyn core::fmt::Debug) {
    if matches!(signal, 10 | 12 | 14 | 17 | 23 | 28 | 29) || signal >= 32 {
        return;
    }
    if !watched(process) || !take(&SIGNALS) {
        return;
    }
    crate::println!(
        "fxdiag: signal {} to process {} ({}) from {:?} by pid {} tid {}",
        signal,
        process.pid(),
        core::str::from_utf8(&name_of(process)).unwrap_or("?"),
        origin,
        crate::syscall::process::current().map_or(0, |p| p.pid()),
        crate::syscall::thread::current().map_or(0, |t| t.tid())
    );
}

/// A fault's signal forced on the running thread of a watched process.
pub(crate) fn fault_signal(process: &Process, tid: u32, signal: u32, code: i32, address: u64, pc: u64, fatal: bool) {
    if !(fatal || (tid == process.pid() && watched(process))) {
        return;
    }
    if !take(&FAULTS) {
        return;
    }
    crate::println!(
        "fxdiag: fault signal {} code {} addr {:#x} pc {:#x} pid {} tid {} ({}/{}) fatal {}",
        signal,
        code,
        address,
        pc,
        process.pid(),
        tid,
        core::str::from_utf8(&name_of(process)).unwrap_or("?"),
        core::str::from_utf8(&thread_name()).unwrap_or("?"),
        fatal
    );
    if tid == process.pid() {
        print_where(process, "pc", pc);
        print_where(process, "addr", address);
    }
}

/// A call of a watched process that answered `EFAULT`.
pub(crate) fn efault(process: &Process, number: usize, args: &[u64; 6], ip: u64) {
    if !watched(process) || !take(&EFAULTS) {
        return;
    }
    crate::println!(
        "fxdiag: EFAULT call {} args {:#x} {:#x} {:#x} {:#x} pid {} tid {} ({}) ip {:#x}",
        number,
        args[0],
        args[1],
        args[2],
        args[3],
        process.pid(),
        crate::syscall::thread::current().map_or(0, |t| t.tid()),
        core::str::from_utf8(&thread_name()).unwrap_or("?"),
        ip
    );
}

