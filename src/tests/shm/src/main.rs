//! System V shared memory as a program uses it, run as init by `cargo xtask
//! test-shm`: through musl's `shmget`, `shmat`, `shmdt` and `shmctl`, which
//! on 32-bit x86 go through `ipc` (117), as the 32-bit Steam client's glibc
//! does.
//!
//! Each step prints `shm: <step> ok`, and the program ends with
//! `shm: all ok` and status 0, or `shm: FAILED <what>` and status 1.
//!
//! * **info**: `shmctl(0, IPC_INFO)` reports a `shmmax` an 8 MiB frame fits;
//! * **chromium**: Chromium's MIT-SHM sequence between two processes that
//!   share nothing else. A child of uid 1000 makes an 8 MiB private segment
//!   of mode 0600, attaches it, fills it and sends its id; the parent, root
//!   as the X server is, attaches it by id, reads every word and answers in
//!   the last page; the child reads the answer, sees two attaches, removes
//!   the segment while both have it attached, still finds it `SHM_DEST`,
//!   and detaches. The parent then sees one attach, detaches, and the
//!   segment is gone;
//! * **permission**: a process of uid 1000 is refused root's mode-0600
//!   segment -- `shmat` and `IPC_STAT` with `EACCES`, `IPC_RMID` with
//!   `EPERM`, and its key with `EACCES`;
//! * **fork**: a forked child shares its parent's attach, is counted, and
//!   detaches as it exits;
//! * **address**: `SHM_RDONLY` cannot be made writable, an address off
//!   `SHMLBA` is refused without `SHM_RND` and rounded with it, a mapped
//!   address is refused without `SHM_REMAP` and replaced with it, and a
//!   second `shmdt` is `EINVAL`;
//! * **direct** (32-bit x86 only): the direct numbers 395 to 398, which
//!   newer glibc calls in place of `ipc`.
//!
//! Built with `negative-control`, the parent leaves out its last `shmdt` in
//! the chromium step, and the program must fail on that step's last check.

use std::io::Write;

use libc::{c_int, c_void};

/// `IPC_PRIVATE`.
const IPC_PRIVATE: libc::key_t = 0;
/// `IPC_CREAT`.
const IPC_CREAT: c_int = 0o1000;
/// `IPC_EXCL`.
const IPC_EXCL: c_int = 0o2000;
/// `IPC_RMID`.
const IPC_RMID: c_int = 0;
/// `IPC_STAT`, which musl sends with `IPC_64` on a 32-bit machine.
const IPC_STAT: c_int = 2;
/// `IPC_INFO`.
const IPC_INFO: c_int = 3;
/// `SHM_RDONLY`.
const SHM_RDONLY: c_int = 0o10000;
/// `SHM_RND`.
const SHM_RND: c_int = 0o20000;
/// `SHM_REMAP`.
const SHM_REMAP: c_int = 0o40000;
/// `SHM_DEST`, in `IPC_STAT`'s mode.
const SHM_DEST: u32 = 0o1000;

/// The frame Chromium's pool would make for a 1920x1080 window at four
/// bytes a pixel, rounded to 8 MiB.
const FRAME: usize = 8 << 20;
/// A page.
const PAGE: usize = 4096;
/// The attach alignment: Linux's `SHMLBA`.
const SHMLBA: usize = if cfg!(target_arch = "arm") {
    4 * PAGE
} else {
    PAGE
};
/// The uid and gid the web helper runs as.
const USER: u32 = 1000;
/// A key of the test's own.
const KEY: libc::key_t = 0x5348_4d54;

/// Where `IPC_STAT` puts `shm_segsz` and `shm_nattch`: the kernel's
/// `shmid64_ds`, which musl's `shmid_ds` begins with.
const SEGSZ_AT: usize = if cfg!(target_pointer_width = "64") {
    48
} else {
    36
};
/// See [`SEGSZ_AT`].
const NATTCH_AT: usize = if cfg!(target_pointer_width = "64") {
    88
} else {
    72
};
/// Where the mode is, in the `ipc_perm` at the start.
const MODE_AT: usize = 20;

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

/// Print one line at once, so the serial log has it before the next step.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The last error's number.
fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Room for any `shmid_ds` or `shminfo`, aligned for its words.
#[repr(C, align(8))]
struct Raw([u8; 256]);

impl Raw {
    /// Zeroes.
    fn new() -> Raw {
        Raw([0; 256])
    }

    /// The word of the machine's width at `at`.
    fn word(&self, at: usize) -> u64 {
        let width = size_of::<usize>();
        let mut eight = [0_u8; 8];
        if let (Some(to), Some(from)) = (eight.get_mut(..width), self.0.get(at..at + width)) {
            to.copy_from_slice(from);
        }
        u64::from_le_bytes(eight)
    }

    /// The `u32` at `at`.
    fn u32_at(&self, at: usize) -> u32 {
        let mut four = [0_u8; 4];
        if let Some(from) = self.0.get(at..at + 4) {
            four.copy_from_slice(from);
        }
        u32::from_le_bytes(four)
    }
}

/// `shmctl(id, cmd, raw)`.
fn ctl(id: c_int, cmd: c_int, raw: &mut Raw) -> c_int {
    // SAFETY: raw is larger than any structure the commands write.
    unsafe { libc::shmctl(id, cmd, raw.0.as_mut_ptr().cast()) }
}

/// `IPC_STAT` of `id`: its mode, size and attach count, or the errno.
fn stat(id: c_int) -> Result<(u32, u64, u64), c_int> {
    let mut raw = Raw::new();
    if ctl(id, IPC_STAT, &mut raw) != 0 {
        return Err(errno());
    }
    Ok((raw.u32_at(MODE_AT), raw.word(SEGSZ_AT), raw.word(NATTCH_AT)))
}

/// Remove `id`.
fn remove(id: c_int) -> c_int {
    let mut raw = Raw::new();
    ctl(id, IPC_RMID, &mut raw)
}

/// `shmget(key, size, flags)`.
fn get(key: libc::key_t, size: usize, flags: c_int) -> c_int {
    // SAFETY: shmget takes plain integers.
    unsafe { libc::shmget(key, size, flags) }
}

/// `shmat(id, at, flags)`, or `None` with the errno set.
fn attach(id: c_int, at: usize, flags: c_int) -> Option<*mut u8> {
    // SAFETY: shmat maps the segment; an address is only a request.
    let mapped = unsafe { libc::shmat(id, at as *const c_void, flags) };
    (mapped as isize != -1).then_some(mapped.cast())
}

/// `shmdt(at)`.
fn detach(at: *mut u8) -> c_int {
    // SAFETY: at is an attach of this process's, or the call refuses it.
    unsafe { libc::shmdt(at.cast()) }
}

/// Fork, running `child` in the child, which ends with its answer. The pid.
fn fork_with(child: impl FnOnce() -> c_int) -> Result<libc::pid_t, String> {
    // SAFETY: the child calls only libc functions and `_exit`s.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork failed, errno {}", errno()));
    }
    if pid == 0 {
        let status = child();
        // SAFETY: ends the child without running the parent's destructors.
        unsafe { libc::_exit(status) };
    }
    Ok(pid)
}

/// Wait for `pid` and return its exit status, or 1000 plus the signal that
/// ended it.
fn reap(pid: libc::pid_t) -> c_int {
    let mut status = 0;
    // SAFETY: status is a live int.
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1000 + libc::WTERMSIG(status)
    }
}

/// Become uid and gid [`USER`]: the web helper.
fn become_user() -> bool {
    // SAFETY: plain integers; the group first, while still root.
    unsafe { libc::setgid(USER) == 0 && libc::setuid(USER) == 0 }
}

/// A pipe: its read and write ends.
fn pipe() -> Result<(c_int, c_int), String> {
    let mut ends = [0; 2];
    // SAFETY: two ints for the two ends.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return Err(format!("pipe failed, errno {}", errno()));
    }
    Ok((ends[0], ends[1]))
}

/// Write `value` to `fd`.
fn send(fd: c_int, value: i32) -> bool {
    let bytes = value.to_le_bytes();
    // SAFETY: four bytes, alive for the call.
    unsafe { libc::write(fd, bytes.as_ptr().cast(), 4) == 4 }
}

/// Read an `i32` from `fd`.
fn receive(fd: c_int) -> Option<i32> {
    let mut bytes = [0_u8; 4];
    // SAFETY: four bytes, alive for the call.
    let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), 4) };
    (read == 4).then(|| i32::from_le_bytes(bytes))
}

/// The word the frame holds at index `index`.
fn pattern(index: usize) -> u32 {
    (index as u32).wrapping_mul(2_654_435_761) ^ 0x5348_4d21
}

/// **info**.
fn info() -> Step {
    let mut raw = Raw::new();
    let answer = ctl(0, IPC_INFO, &mut raw);
    let shmmax = raw.word(0);
    if answer < 0 || shmmax < FRAME as u64 {
        return Err(format!(
            "IPC_INFO answered {answer}, errno {}, shmmax {shmmax}",
            errno()
        ));
    }
    Ok(())
}

/// **chromium**'s web helper side, as uid 1000; nonzero is the step that
/// failed.
fn web_helper(to_server: c_int, from_server: c_int) -> c_int {
    if !become_user() {
        return 2;
    }
    let id = get(IPC_PRIVATE, FRAME, IPC_CREAT | 0o600);
    if id < 0 {
        return 3;
    }
    let Some(frame) = attach(id, 0, 0) else {
        return 4;
    };
    let words = frame.cast::<u32>();
    for index in 0..FRAME / 4 {
        // SAFETY: inside the 8 MiB attach.
        unsafe { words.add(index).write_volatile(pattern(index)) };
    }
    if !send(to_server, id) || receive(from_server) != Some(1) {
        return 5;
    }
    let answer_at = (FRAME - PAGE) / 4;
    // SAFETY: inside the attach; the server wrote it before it answered.
    let answer = unsafe { words.add(answer_at).read_volatile() };
    if answer != !pattern(answer_at) {
        return 6;
    }
    match stat(id) {
        Ok((mode, size, 2)) if mode & 0o777 == 0o600 && size == FRAME as u64 => {}
        _ => return 7,
    }
    // As Chromium does once the X server has attached it.
    if remove(id) != 0 {
        return 8;
    }
    match stat(id) {
        Ok((mode, _, 2)) if mode & SHM_DEST != 0 => {}
        _ => return 9,
    }
    if detach(frame) != 0 {
        return 10;
    }
    match stat(id) {
        Ok((_, _, 1)) => 0,
        _ => 11,
    }
}

/// **chromium**.
fn chromium() -> Step {
    let (server_reads, helper_writes) = pipe()?;
    let (helper_reads, server_writes) = pipe()?;
    let helper = fork_with(|| {
        let step = web_helper(helper_writes, helper_reads);
        if step != 0 {
            say(&format!(
                "shm: the web helper failed at {step}, errno {}",
                errno()
            ));
        }
        step
    })?;
    let Some(id) = receive(server_reads) else {
        return Err(format!(
            "the web helper sent no id; it ended {}",
            reap(helper)
        ));
    };
    // The X server, as root, attaches a segment of mode 0600 it does not own.
    let Some(frame) = attach(id, 0, 0) else {
        return Err(format!(
            "root could not attach the helper's segment, errno {}",
            errno()
        ));
    };
    let words = frame.cast::<u32>();
    for index in 0..FRAME / 4 {
        // SAFETY: inside the attach.
        let seen = unsafe { words.add(index).read_volatile() };
        if seen != pattern(index) {
            return Err(format!("word {index} read {seen:#x} across the processes"));
        }
    }
    let answer_at = (FRAME - PAGE) / 4;
    // SAFETY: inside the attach.
    unsafe { words.add(answer_at).write_volatile(!pattern(answer_at)) };
    if !send(server_writes, 1) {
        return Err("could not answer the web helper".into());
    }
    let status = reap(helper);
    if status != 0 {
        return Err(format!("the web helper ended {status}"));
    }
    match stat(id) {
        Ok((mode, _, 1)) if mode & SHM_DEST != 0 => {}
        other => {
            return Err(format!(
                "after the helper's detach, IPC_STAT read {other:?}, not one attach and SHM_DEST"
            ));
        }
    }
    if !cfg!(feature = "negative-control") && detach(frame) != 0 {
        return Err(format!("the server's shmdt failed, errno {}", errno()));
    }
    match stat(id) {
        Err(libc::EINVAL) => Ok(()),
        other => Err(format!(
            "the segment outlived its last detach: IPC_STAT read {other:?}"
        )),
    }
}

/// **permission**'s child, as uid 1000; nonzero is the step that failed.
fn intruder(id: c_int) -> c_int {
    if !become_user() {
        return 2;
    }
    if attach(id, 0, SHM_RDONLY).is_some() || errno() != libc::EACCES {
        return 3;
    }
    if stat(id) != Err(libc::EACCES) {
        return 4;
    }
    if remove(id) != -1 || errno() != libc::EPERM {
        return 5;
    }
    if get(KEY, 0, 0o600) != -1 || errno() != libc::EACCES {
        return 6;
    }
    0
}

/// **permission**.
fn permission() -> Step {
    let id = get(IPC_PRIVATE, PAGE, IPC_CREAT | 0o600);
    let keyed = get(KEY, PAGE, IPC_CREAT | IPC_EXCL | 0o600);
    if id < 0 || keyed < 0 {
        return Err(format!("shmget failed, errno {}", errno()));
    }
    let child = fork_with(|| intruder(id))?;
    let status = reap(child);
    let removed = remove(id) == 0 && remove(keyed) == 0;
    if status != 0 {
        return Err(format!("uid 1000 was let in at step {status}"));
    }
    if !removed {
        return Err("root could not remove its segments".into());
    }
    Ok(())
}

/// **fork**.
fn fork() -> Step {
    let id = get(IPC_PRIVATE, PAGE, IPC_CREAT | 0o600);
    if id < 0 {
        return Err(format!("shmget failed, errno {}", errno()));
    }
    let Some(page) = attach(id, 0, 0) else {
        return Err(format!("shmat failed, errno {}", errno()));
    };
    let word = page.cast::<u32>();
    // SAFETY: inside the attach.
    unsafe { word.write_volatile(1) };
    // Removed at once: it stays for as long as either process has it.
    if remove(id) != 0 {
        return Err("IPC_RMID failed".into());
    }
    let child = fork_with(|| {
        // SAFETY: the attach, inherited.
        let seen = unsafe { word.read_volatile() };
        // SAFETY: as above.
        unsafe { word.write_volatile(2) };
        match stat(id) {
            Ok((_, _, 2)) if seen == 1 => 0,
            _ => 2,
        }
    })?;
    let status = reap(child);
    // SAFETY: the parent's attach.
    let seen = unsafe { word.read_volatile() };
    if status != 0 || seen != 2 {
        return Err(format!("the child ended {status}; the parent read {seen}"));
    }
    match stat(id) {
        Ok((_, _, 1)) => {}
        other => return Err(format!("after the child's exit IPC_STAT read {other:?}")),
    }
    if detach(page) != 0 || stat(id) != Err(libc::EINVAL) {
        return Err("the segment did not go with its last detach".into());
    }
    Ok(())
}

/// **address**.
fn address() -> Step {
    let len = 3 * PAGE;
    let id = get(IPC_PRIVATE, len, IPC_CREAT | 0o600);
    if id < 0 {
        return Err(format!("shmget failed, errno {}", errno()));
    }
    let outcome = address_with(id, len);
    let _ = remove(id);
    outcome
}

/// **address**, on segment `id` of `len` bytes.
fn address_with(id: c_int, len: usize) -> Step {
    let Some(read_only) = attach(id, 0, SHM_RDONLY) else {
        return Err(format!("SHM_RDONLY failed, errno {}", errno()));
    };
    // SAFETY: the read-only attach; mprotect only asks.
    let widened =
        unsafe { libc::mprotect(read_only.cast(), len, libc::PROT_READ | libc::PROT_WRITE) };
    if widened != -1 || errno() != libc::EACCES {
        return Err(format!("a SHM_RDONLY attach was made writable: {widened}"));
    }
    if detach(read_only) != 0 {
        return Err("shmdt of the read-only attach failed".into());
    }
    let room = 8 * SHMLBA;
    // SAFETY: a reservation of the test's own.
    let reserved = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            room,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if reserved == libc::MAP_FAILED {
        return Err("no room to attach at".into());
    }
    let base = (reserved as usize).next_multiple_of(SHMLBA);
    let outcome = fixed(id, base);
    // SAFETY: the reservation, whatever is in it now.
    let _ = unsafe { libc::munmap(reserved, room) };
    outcome
}

/// The fixed-address cases at `base`, an `SHMLBA` boundary inside a
/// reservation.
fn fixed(id: c_int, base: usize) -> Step {
    if attach(id, base, 0).is_some() || errno() != libc::EINVAL {
        return Err("an attach over a mapping without SHM_REMAP was not EINVAL".into());
    }
    if attach(id, base + 100, 0).is_some() || errno() != libc::EINVAL {
        return Err("an attach off SHMLBA without SHM_RND was not EINVAL".into());
    }
    let Some(at) = attach(id, base + 100, SHM_RND | SHM_REMAP) else {
        return Err(format!("SHM_RND | SHM_REMAP failed, errno {}", errno()));
    };
    if at as usize != base {
        return Err(format!("SHM_RND attached at {at:p}, not {base:#x}"));
    }
    // SAFETY: inside the attach.
    unsafe { at.write_volatile(7) };
    if detach(at) != 0 {
        return Err("shmdt failed".into());
    }
    if detach(at) != -1 || errno() != libc::EINVAL {
        return Err("a second shmdt was not EINVAL".into());
    }
    Ok(())
}

/// **direct**: the numbers Linux 5.1 gave i386, which glibc 2.35 and later
/// call where it has them. Run on 32-bit x86 only: elsewhere the numbers
/// are other calls.
fn direct() -> Step {
    const SHMGET: libc::c_long = 395;
    const SHMCTL: libc::c_long = 396;
    const SHMAT: libc::c_long = 397;
    const SHMDT: libc::c_long = 398;
    const IPC_64: c_int = 0x100;
    // SAFETY: plain integers.
    let id = unsafe { libc::syscall(SHMGET, IPC_PRIVATE, PAGE, IPC_CREAT | 0o600) };
    if id < 0 {
        return Err(format!("shmget (395) failed, errno {}", errno()));
    }
    let id = id as c_int;
    // SAFETY: as shmat.
    let at = unsafe { libc::syscall(SHMAT, id, 0, 0) };
    if at == -1 {
        return Err(format!("shmat (397) failed, errno {}", errno()));
    }
    let mut raw = Raw::new();
    // SAFETY: raw is larger than the 84-byte shmid64_ds.
    let answer = unsafe { libc::syscall(SHMCTL, id, IPC_STAT | IPC_64, raw.0.as_mut_ptr()) };
    let attaches = raw.word(NATTCH_AT);
    // SAFETY: the attach.
    let detached = unsafe { libc::syscall(SHMDT, at) };
    // SAFETY: plain integers.
    let removed = unsafe { libc::syscall(SHMCTL, id, IPC_RMID, 0) };
    if answer != 0 || attaches != 1 || detached != 0 || removed != 0 {
        return Err(format!(
            "IPC_STAT (396) {answer} read {attaches} attaches; shmdt (398) {detached}; \
             IPC_RMID {removed}"
        ));
    }
    Ok(())
}

fn main() {
    let steps: [Named; 5] = [
        ("info", info),
        ("chromium", chromium),
        ("permission", permission),
        ("fork", fork),
        ("address", address),
    ];
    let direct_step: &[Named] = if cfg!(target_arch = "x86") {
        &[("direct", direct)]
    } else {
        &[]
    };
    for &(name, step) in steps.iter().chain(direct_step) {
        if let Err(what) = step() {
            say(&format!("shm: FAILED {name}: {what}"));
            std::process::exit(1);
        }
        say(&format!("shm: {name} ok"));
    }
    say("shm: all ok");
}
