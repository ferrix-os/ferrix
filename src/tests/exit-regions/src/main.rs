//! A process of tens of thousands of regions, ended by `exit` and by
//! `SIGKILL`, run as init by `cargo xtask test-exit-regions`.
//!
//! On 2026-11-12 the N150 box reset itself, twice with nothing recorded,
//! when a process holding gigabytes in thousands of regions ended: the log
//! reader `lode` loading a whole journal into a heap of 256 KiB chunks and
//! killed by `SIGPIPE`, and the game, of 12,000 to 16,000 regions, killed with
//! `kill -9`. The ending task gives its whole address space back in one
//! unmap on its own kernel stack, and that unmap sorted one id per region
//! with a recursive sort, which overflowed the stack into its guard page at
//! about 36,000 regions (`docs/certification/FINDINGS.md`).
//!
//! This program makes the shape twice, each time in a child it forks:
//! [`REGIONS`] anonymous regions of [`REGION_BYTES`], every page touched,
//! each asked for a page past the last one's end, so that no two can be one
//! region. Then:
//!
//! * **exit**: the child calls `_exit(0)`;
//! * **kill**: the child waits in `pause`, and is sent `SIGKILL`;
//! * **sigpipe**: the child is blocked writing to a pipe whose reader then
//!   goes, and `SIGPIPE` ends it on its way out of the write.
//! * **enomem**: a child maps regions until `mmap` refuses it, and exits. Its
//!   memory must be back while it is a zombie (`waitid` with `WNOWAIT`) and
//!   at `waitpid`'s return, within the larger of 16 MiB and 2% of MemFree
//!   before, and its VmRSS as a zombie must be 0 (F-114, L.user.247).
//!
//! The parent waits for the child and prints, per phase, `exitregions:
//! <phase> regions <n> MiB <m> teardown <ms> ms canary worst <ms> ms late`:
//! the time from the end to `waitpid`'s return, which is the time the
//! kernel took to give the child's memory back (it does before a waiter is
//! told), and how late a 10 ms sleeper in the parent woke at worst in that
//! time. The program ends with `exitregions: done` and status 0; any step
//! that fails prints `exitregions: FAILED <why>` and status 1. Whether the
//! figures are within a bound is the row's to say
//! (`tools/common/xtask/src/exit_regions.rs`).

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Regions the child makes: 60,000 on x86-64; 40,000 on the Arm pair,
/// which is still more than the 36,864 the overflow was first met at on
/// x86-64 (consultant ledger 9112 E5). AArch64's region map refuses to grow
/// past the largest kernel heap request (4 MiB, `MAX_ORDER` 10) at 47,083
/// regions, and ARMv7-A's 1.2 GiB guest refused the 45,036th: neither holds
/// 60,000 in the row's machine.
const REGIONS: usize = if cfg!(target_arch = "x86_64") {
    60_000
} else {
    40_000
};
/// Bytes in each region: four pages, every one touched.
const REGION_BYTES: usize = 16 * 1024;
/// A page.
const PAGE: usize = 4096;
/// How long the canary sleeps.
const NAP: Duration = Duration::from_millis(10);

/// What the canary shares with the phases.
#[derive(Debug, Default)]
struct Canary {
    /// Set while a teardown is being measured.
    measuring: AtomicBool,
    /// The latest wake while measuring, in microseconds.
    worst_us: AtomicU64,
    /// Set when the canary is to stop.
    stop: AtomicBool,
}

fn main() {
    let words: Vec<String> = std::env::args().collect();
    if let [_, role, name, end, ready, data] = words.as_slice()
        && role == "child"
    {
        let end = match end.as_str() {
            "exit" => End::Exit,
            "kill" => End::Kill,
            "enomem" => End::Enomem,
            _ => End::Pipe,
        };
        child(
            name,
            end,
            ready.parse().unwrap_or(-1),
            data.parse().unwrap_or(-1),
        );
    }
    let canary = Arc::new(Canary::default());
    let sleeper = {
        let canary = Arc::clone(&canary);
        std::thread::spawn(move || {
            while !canary.stop.load(Ordering::Relaxed) {
                let asleep = Instant::now();
                std::thread::sleep(NAP);
                let late = asleep.elapsed().saturating_sub(NAP);
                if canary.measuring.load(Ordering::Relaxed) {
                    let late = u64::try_from(late.as_micros()).unwrap_or(u64::MAX);
                    let _ = canary.worst_us.fetch_max(late, Ordering::Relaxed);
                }
            }
        })
    };
    let result = phase("exit", End::Exit, &canary)
        .and_then(|()| phase("kill", End::Kill, &canary))
        .and_then(|()| phase("sigpipe", End::Pipe, &canary))
        .and_then(|()| enomem());
    canary.stop.store(true, Ordering::Relaxed);
    let _ = sleeper.join();
    match result {
        Ok(()) => {
            println!("exitregions: done");
            leave(0);
        }
        Err(why) => {
            println!("exitregions: FAILED {why}");
            leave(1);
        }
    }
}

/// Flush and exit with `code`.
fn leave(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

/// The last error, as text.
fn errno() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Map region `index` at `hint` if it is free, and touch every page of it:
/// answers where it went.
fn region(index: usize, hint: usize) -> usize {
    // SAFETY: an anonymous private mapping the kernel places, at the
    // hint if it is free; no memory of ours is named.
    let at = unsafe {
        libc::mmap(
            hint as *mut libc::c_void,
            REGION_BYTES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        // Said without the heap, which may be refused for the same reason.
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        say_unallocated(b"exitregions: FAILED mmap of region ", index, code);
        // SAFETY: ends this process, with nothing of it to unwind.
        unsafe { libc::_exit(3) }
    }
    let base = at.cast::<u8>();
    for offset in (0..REGION_BYTES).step_by(PAGE) {
        // SAFETY: inside the mapping just made, which is readable and
        // writable and ours alone.
        unsafe { base.add(offset).write_volatile(1) };
    }
    base as usize
}

/// Where the first region is asked for: far from the program, its heap and
/// its stack on both pointer widths, so the hints are free.
#[cfg(target_pointer_width = "64")]
const FIRST_HINT: usize = 0x2000_0000_0000;
/// Where the first region is asked for in a 2 GiB user half.
#[cfg(not(target_pointer_width = "64"))]
const FIRST_HINT: usize = 0x2000_0000;

/// Make the regions in the calling process, each asked for a page past the
/// last one's end, so that they go up in address as their objects were made,
/// a page apart, as a heap's chunks and the box's reader's did: answers the
/// bytes they hold.
fn fill() -> Result<usize, String> {
    // One mapping above where the run will go, made first, and split in
    // three by protecting its middle page: three regions naming one object
    // made before all of the run's, after the run in address order.
    let above = FIRST_HINT + REGIONS * (REGION_BYTES + PAGE) + 1024 * 1024;
    // SAFETY: as each region's below.
    let split = unsafe {
        libc::mmap(
            above as *mut libc::c_void,
            3 * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if split == libc::MAP_FAILED {
        return Err(format!("mmap of the split mapping: {}", errno()));
    }
    // SAFETY: the middle page of the mapping just made.
    if unsafe { libc::mprotect(split.cast::<u8>().add(PAGE).cast(), PAGE, libc::PROT_READ) } != 0 {
        return Err(format!("mprotect of the split mapping: {}", errno()));
    }
    // The run's first region is made second and its second first, so that
    // the run begins out of order, as the box's reader's and the hog's did.
    let stride = REGION_BYTES + PAGE;
    for (index, at) in [(1, FIRST_HINT + stride), (0, FIRST_HINT)] {
        let _ = region(index, at);
    }
    let mut hint = FIRST_HINT + 2 * stride;
    for index in 2..REGIONS {
        let at = region(index, hint);
        hint = at.saturating_add(stride);
    }
    Ok(REGIONS * REGION_BYTES)
}

/// Map regions of [`REGION_BYTES`], every page touched, each a page past
/// the last, until `mmap` refuses one; say where on standard output and on
/// `ready`, from buffers on the stack, since the heap may be refused too, and
/// end with `_exit(0)`.
fn fill_until_refused(ready: i32) -> ! {
    let stride = REGION_BYTES + PAGE;
    let mut hint = FIRST_HINT;
    let mut index = 0_usize;
    let code = loop {
        // SAFETY: as `region`'s: an anonymous private mapping the kernel
        // places; no memory of ours is named.
        let at = unsafe {
            libc::mmap(
                hint as *mut libc::c_void,
                REGION_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            break std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        }
        let base = at.cast::<u8>();
        for offset in (0..REGION_BYTES).step_by(PAGE) {
            // SAFETY: inside the mapping just made, ours alone.
            unsafe { base.add(offset).write_volatile(1) };
        }
        hint = (base as usize).saturating_add(stride);
        index += 1;
    };
    say_unallocated(
        b"exitregions: enomem: the child was refused at region ",
        index,
        code,
    );
    // SAFETY: one byte from a live buffer to the pipe's write end.
    let _ = unsafe { libc::write(ready, b"r".as_ptr().cast(), 1) };
    // SAFETY: ends this process, with nothing of it to unwind.
    unsafe { libc::_exit(0) }
}

/// The kB figure on the line of `text` that starts with `field`, if there is
/// one: what the enomem phase reads, failing where a figure is missing
/// rather than reading it as 0 (ledger 9740 NR.5).
fn kb_read(text: &str, field: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|figure| figure.parse().ok())
}

/// `/proc/meminfo`'s `field` in kB, or the phase's error naming it.
fn meminfo_needed(name: &str, field: &str) -> Result<u64, String> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("{name}: /proc/meminfo: {error}"))?;
    kb_read(&text, field).ok_or_else(|| format!("{name}: /proc/meminfo has no {field} figure"))
}

/// The enomem phase: a child maps until refused and exits; its memory must
/// be back while it is a zombie, before it is reaped, and at `waitpid`'s
/// return, within the larger of 16 MiB and 2% of MemFree before (ledger
/// 9543 ET.5, F-114), and the zombie's VmRSS must be 0.
fn enomem() -> Result<(), String> {
    let name = "enomem";
    println!("exitregions: {name}: before, {}", memory());
    let before = meminfo_needed(name, "MemFree:")?;
    let total = meminfo_needed(name, "MemTotal:")?;
    let (ready_read, ready_write) = pipe(name)?;
    let (data_read, data_write) = pipe(name)?;
    // SAFETY: the child execs or runs `child`, which maps, writes and ends.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("{name}: fork: {}", errno()));
    }
    if pid == 0 {
        close(ready_read);
        close(data_read);
        exec_child(name, End::Enomem, ready_write, data_write);
    }
    close(ready_write);
    close(data_write);
    close(data_read);
    let mut byte = [0_u8; 1];
    // SAFETY: one byte into a live buffer.
    let got = unsafe { libc::read(ready_read, byte.as_mut_ptr().cast(), 1) };
    close(ready_read);
    // SAFETY: a zeroed siginfo is a valid one to be written.
    let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
    // SAFETY: the child forked above; WNOWAIT leaves it a zombie.
    let waited = unsafe {
        libc::waitid(
            libc::P_PID,
            libc::id_t::try_from(pid).unwrap_or(0),
            &raw mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    if waited != 0 {
        return Err(format!("{name}: waitid: {}", errno()));
    }
    let zombie = meminfo_needed(name, "MemFree:")?;
    let status_text = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|error| format!("{name}: the zombie's status: {error}"))?;
    let zombie_rss = kb_read(&status_text, "VmRSS:")
        .ok_or_else(|| format!("{name}: the zombie's status has no VmRSS line"))?;
    let mut status = 0;
    // SAFETY: the child forked above, and room for its status.
    let reaped = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    let after = meminfo_needed(name, "MemFree:")?;
    println!("exitregions: {name}: at waitpid's return, {}", memory());
    if reaped != pid || got != 1 {
        return Err(format!(
            "{name}: the child did not map until refused (status {status:#x})"
        ));
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(format!("{name}: the child ended with status {status:#x}"));
    }
    let margin = (16 * 1024).max(before / 50);
    let short = |now: u64| before.saturating_sub(now);
    println!(
        "exitregions: {name} guest {} MiB MemFree before {before} kB zombie {zombie} kB at return \
         {after} kB margin {margin} kB zombie VmRSS {zombie_rss} kB",
        total >> 10
    );
    if short(zombie) > margin || short(after) > margin || zombie_rss != 0 {
        return Err(format!(
            "{name}: the child's memory was not back before it was reaped: {} kB short as a \
             zombie, {} kB at waitpid's return, VmRSS {zombie_rss} kB",
            short(zombie),
            short(after)
        ));
    }
    println!("exitregions: {name} ok");
    let _ = std::io::stdout().flush();
    Ok(())
}

/// How a phase's child ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// It calls `_exit(0)`.
    Exit,
    /// It waits in `pause` and is sent `SIGKILL`.
    Kill,
    /// It writes to a pipe whose reader goes, and `SIGPIPE` ends it on the
    /// way out of the write: the box's `lode | head`.
    Pipe,
    /// It maps regions until `mmap` refuses it, then calls `_exit(0)`: an end
    /// with no room left for anything (F-114).
    Enomem,
}

/// How long the parent lets the child settle in its last call before it
/// ends it: the child is then asleep in `pause`, or blocked on a full pipe.
const SETTLE: Duration = Duration::from_millis(300);

/// The child's life: make the regions, say so on `ready`, then end as `end`
/// says, writing to `data` for [`End::Pipe`]. Never returns.
fn child(name: &str, end: End, ready: i32, data: i32) -> ! {
    if end == End::Enomem {
        fill_until_refused(ready);
    }
    match fill() {
        Ok(_) => {
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
            println!(
                "exitregions: {name}: the child has {} regions",
                maps.lines().count()
            );
            let _ = std::io::stdout().flush();
            // SAFETY: one byte from a live buffer to the pipe's write end.
            let _ = unsafe { libc::write(ready, b"r".as_ptr().cast(), 1) };
            match end {
                End::Exit | End::Enomem => {}
                End::Kill => loop {
                    // SAFETY: no arguments; it returns only on a signal.
                    let _ = unsafe { libc::pause() };
                },
                End::Pipe => {
                    // std ignores SIGPIPE; the box's reader did not.
                    // SAFETY: the default action back, for this one signal.
                    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
                    let chunk = [b'x'; 4096];
                    loop {
                        // SAFETY: a live buffer of its own length.
                        let _ = unsafe { libc::write(data, chunk.as_ptr().cast(), chunk.len()) };
                    }
                }
            }
            // SAFETY: ends this process, with nothing of it to unwind.
            unsafe { libc::_exit(0) }
        }
        Err(why) => {
            println!("exitregions: FAILED {name}: the child: {why}");
            let _ = std::io::stdout().flush();
            // SAFETY: as above.
            unsafe { libc::_exit(2) }
        }
    }
}

/// Run [`child`] in a fresh image of this program, so that its regions are
/// a new process's, as the box's were, and not a fork's copy of the parent's.
/// `cargo xtask test-exit-regions` carries the program there; without it
/// the forked child runs [`child`] itself.
fn exec_child(name: &str, end: End, ready: i32, data: i32) -> ! {
    let end_word = match end {
        End::Exit => "exit",
        End::Kill => "kill",
        End::Pipe => "pipe",
        End::Enomem => "enomem",
    };
    let words = [
        "exit-regions-test".to_owned(),
        "child".to_owned(),
        name.to_owned(),
        end_word.to_owned(),
        ready.to_string(),
        data.to_string(),
    ];
    let owned: Vec<std::ffi::CString> = words
        .iter()
        .filter_map(|word| std::ffi::CString::new(word.as_str()).ok())
        .collect();
    let mut argv: Vec<*const libc::c_char> = owned.iter().map(|word| word.as_ptr()).collect();
    argv.push(core::ptr::null());
    // SAFETY: a NUL-terminated path and a NULL-terminated vector of
    // NUL-terminated words that live across the call.
    let _ = unsafe { libc::execv(c"/bin/exit-regions-test".as_ptr(), argv.as_ptr()) };
    println!(
        "exitregions: {name}: no exec of /bin/exit-regions-test ({}), the forked child runs it",
        errno()
    );
    child(name, end, ready, data)
}

/// MemFree and MemAvailable, as `/proc/meminfo` says them, for the log.
fn memory() -> String {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    text.lines()
        .filter(|line| {
            line.starts_with("MemFree")
                || line.starts_with("MemAvailable")
                || line.starts_with("Slab")
        })
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Write `prefix`, `index`, " errno " and `code` and a newline to standard
/// output from a buffer on the stack: no allocation.
fn say_unallocated(prefix: &[u8], index: usize, code: i32) {
    let mut line = [0_u8; 128];
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        for &byte in bytes {
            if let Some(slot) = line.get_mut(at) {
                *slot = byte;
                at += 1;
            }
        }
    };
    put(prefix);
    put(decimal(index as u64).as_slice());
    put(b" errno ");
    put(decimal(u64::try_from(code).unwrap_or(0)).as_slice());
    put(b"\n");
    // SAFETY: the first `at` bytes of a live buffer.
    let _ = unsafe { libc::write(1, line.as_ptr().cast(), at) };
}

/// `value` in decimal, in a fixed buffer: its digits, without the heap.
fn decimal(mut value: u64) -> DecimalDigits {
    let mut digits = [0_u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        if let Some(slot) = digits.get_mut(start) {
            *slot = b'0' + (value % 10) as u8;
        }
        value /= 10;
        if value == 0 || start == 0 {
            break;
        }
    }
    DecimalDigits { digits, start }
}

/// What [`decimal`] answers.
#[derive(Debug)]
struct DecimalDigits {
    /// The digits, right-aligned.
    digits: [u8; 20],
    /// Where they begin.
    start: usize,
}

impl DecimalDigits {
    /// The digits.
    fn as_slice(&self) -> &[u8] {
        self.digits.get(self.start..).unwrap_or_default()
    }
}

/// A pipe's two ends.
fn pipe(name: &str) -> Result<(i32, i32), String> {
    let mut fds = [0_i32; 2];
    // SAFETY: room for the two descriptors the call writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("{name}: pipe: {}", errno()));
    }
    let [read_end, write_end] = fds;
    Ok((read_end, write_end))
}

/// Close `fd`.
fn close(fd: i32) {
    // SAFETY: a descriptor this process opened and no longer uses.
    let _ = unsafe { libc::close(fd) };
}

/// One phase: a child of [`REGIONS`] regions, ended as `end` says, and the
/// teardown timed from the end to `waitpid`'s return.
fn phase(name: &str, end: End, canary: &Canary) -> Result<(), String> {
    println!("exitregions: {name}: before, {}", memory());
    let (ready_read, ready_write) = pipe(name)?;
    let (data_read, data_write) = pipe(name)?;
    // SAFETY: the child runs only `child`, which makes mappings, writes and
    // ends; the parent's other thread holds no lock it needs.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("{name}: fork: {}", errno()));
    }
    if pid == 0 {
        close(ready_read);
        close(data_read);
        exec_child(name, end, ready_write, data_write);
    }
    close(ready_write);
    close(data_write);
    let mut byte = [0_u8; 1];
    // SAFETY: one byte into a live buffer.
    let got = unsafe { libc::read(ready_read, byte.as_mut_ptr().cast(), 1) };
    close(ready_read);
    if got == 1 && end != End::Exit {
        std::thread::sleep(SETTLE);
    }
    canary.worst_us.store(0, Ordering::Relaxed);
    canary.measuring.store(true, Ordering::Relaxed);
    let began = Instant::now();
    if got == 1 && end == End::Kill {
        // SAFETY: the child forked above, not yet waited for.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            return Err(format!("{name}: kill: {}", errno()));
        }
    }
    // The reader goes: the child's blocked write fails, and SIGPIPE ends it.
    close(data_read);
    let mut status = 0;
    // SAFETY: the child forked above, and room for its status.
    let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    let took = began.elapsed();
    println!("exitregions: {name}: at waitpid's return, {}", memory());
    canary.measuring.store(false, Ordering::Relaxed);
    if waited != pid {
        return Err(format!("{name}: waitpid: {}", errno()));
    }
    if got != 1 {
        return Err(format!(
            "{name}: the child ended before its regions were made (status {status:#x})"
        ));
    }
    let ended_as_asked = match end {
        End::Exit | End::Enomem => libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        End::Kill => libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL,
        End::Pipe => libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGPIPE,
    };
    if !ended_as_asked {
        return Err(format!("{name}: the child ended with status {status:#x}"));
    }
    let worst = canary.worst_us.load(Ordering::Relaxed);
    println!(
        "exitregions: {name} regions {REGIONS} MiB {} teardown {} ms canary worst {}.{:02} ms late",
        (REGIONS * REGION_BYTES) >> 20,
        took.as_millis(),
        worst / 1000,
        worst % 1000 / 10
    );
    let _ = std::io::stdout().flush();
    Ok(())
}
