//! The input thread: the devices read where no frame can hold them
//! (`docs/GAME-PATH.md` §4.7, stage 24's I1).
//!
//! The compositor's loop reads, draws and waits in one thread, and a frame
//! through `virgl_test_server` takes 9 to 14 ms: input that arrived while one
//! was drawn waited for it. So the devices are read on a thread of their own,
//! which does two things with what it reads:
//!
//! * **A locked pointer's motion goes to its client at once.** While a
//!   client holds a pointer lock on the surface under the pointer, the loop
//!   names that client's `zwp_relative_pointer_v1`s and its connection here
//!   ([`Front::set_lane`]), and the thread writes each relative motion to
//!   them itself, through the connection's [`Sender`]. A lock needs no
//!   decision from the compositor: the pointer does not move and no window
//!   is chosen, so nothing the loop knows is wanted for it.
//! * **Everything else is queued for the loop**, which is woken through a
//!   pipe, and is carried out there as it always was: keys, buttons, the
//!   pointer over the desktop, and the forwarded motions too, marked, so
//!   the seat still sees the input (idle, the drawn pointer) and sends no
//!   second copy.
//!
//! # Which objects the thread may name
//!
//! A relative pointer the client destroys is answered with
//! `wl_display.delete_id`, after which the client may reuse its id for
//! another object. The thread must never write to the id after that. The
//! loop takes the lane's lock and takes the id out of the lane *before*
//! the client's queued messages -- the `delete_id` among them -- go into
//! the connection's queue ([`Front::sync_lane`] from `Slot::flush`), and the
//! thread writes only while holding the same lock; so every motion it
//! wrote to the id is in the socket ahead of the `delete_id`.
//!
//! # On request
//!
//! `HYPRIX_INPUT_AGES=1` says, for every pointer motion as it passes to a
//! client, `hyprix: input-age stamp <us> age <us> lane|loop`: the report's
//! stamp (`CLOCK_MONOTONIC`) and how old it was when it was written. The
//! probe pairs these with yserver's and its own (`xlatency hops`).
//! `HYPRIX_INPUT_THREAD=0` reads the devices on the loop as before, which is
//! the boot check's negative control; `HYPRIX_HOLD_FRAME_MS=<n>`
//! (`crate::knobs`) holds every frame that long after its frame callbacks
//! are sent, which is the frame the check moves the mouse under.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use compositor_socket::Sender;
use compositor_wire::ObjectId;

use crate::devices::{Devices, Stamped};
use crate::seat::Input;

/// How long the thread waits on its devices before it looks again at which
/// are open: a device a rescan opened is read from then on.
const LOOK_AGAIN: Duration = Duration::from_millis(100);

/// One input for the loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Taken {
    /// The input.
    pub input: Input,
    /// Its report's stamp, microseconds of `CLOCK_MONOTONIC`; 0 when none.
    pub at: u64,
    /// Whether the thread already wrote it to the locked client: the seat
    /// takes it, and no client is sent it again.
    pub forwarded: bool,
}

/// Where a locked pointer's motion goes.
#[derive(Debug)]
pub struct Lane {
    /// The slot's serial, which the loop's own connection is known by.
    pub serial: u64,
    /// The client's connection.
    pub sender: Sender,
    /// Its relative pointers.
    pub pointers: Vec<ObjectId>,
}

/// What the thread and the loop share.
#[derive(Debug)]
struct Shared {
    devices: Mutex<Devices>,
    queue: Mutex<Vec<Taken>>,
    lane: Mutex<Option<Lane>>,
    said: Mutex<Vec<String>>,
    stop: AtomicBool,
    ages: bool,
    /// The pipe's writing end, which wakes the loop.
    wake: Option<OwnedFd>,
}

/// The devices, read on a thread of their own or on the loop.
#[derive(Debug)]
pub struct Front {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// The pipe's reading end, which the loop waits on.
    woken: Option<OwnedFd>,
}

/// A mutex's guard, whether or not a thread panicked holding it: what each
/// holds is whole between two statements.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `CLOCK_MONOTONIC` in microseconds, the clock the input core stamps with.
#[must_use]
pub fn monotonic_us() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[expect(
        unsafe_code,
        reason = "AUDIT: clock_gettime writes one timespec into a local"
    )]
    // SAFETY: `now` is a valid, writable timespec for the call's duration.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let micros = u64::try_from(now.tv_nsec).unwrap_or(0) / 1_000;
    seconds.saturating_mul(1_000_000).saturating_add(micros)
}

/// The line an input's age is said in.
#[must_use]
pub fn age_line(at: u64, now: u64, how: &str) -> String {
    format!(
        "hyprix: input-age stamp {at} age {} {how}",
        now.saturating_sub(at)
    )
}

impl Front {
    /// Take the devices, with the thread unless the environment says not
    /// to (the module's comment). A compositor that does not own the screen
    /// has no devices and reads none, and needs no thread.
    #[must_use]
    pub fn new(devices: Devices, owns_screen: bool) -> Self {
        let wanted = std::env::var("HYPRIX_INPUT_THREAD").map_or(true, |value| value != "0");
        let ages = std::env::var("HYPRIX_INPUT_AGES").is_ok_and(|value| value == "1");
        Self::with(devices, wanted && owns_screen, ages)
    }

    /// The same, said outright.
    #[must_use]
    pub fn with(devices: Devices, threaded: bool, ages: bool) -> Self {
        let pipe = if threaded { pipe() } else { None };
        let (woken, wake) = pipe.map_or((None, None), |(read, write)| (Some(read), Some(write)));
        let shared = Arc::new(Shared {
            devices: Mutex::new(devices),
            queue: Mutex::new(Vec::new()),
            lane: Mutex::new(None),
            said: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            ages,
            wake,
        });
        let thread = woken.as_ref().and_then(|_| {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("hyprix-input".to_owned())
                .spawn(move || run(&shared))
                .ok()
        });
        Self {
            shared,
            // Without the thread there is nothing to wake for.
            woken: thread.as_ref().and(woken),
            thread,
        }
    }

    /// Whether the devices are read on the thread.
    #[must_use]
    pub const fn threaded(&self) -> bool {
        self.thread.is_some()
    }

    /// Whether ages are said.
    #[must_use]
    pub fn ages(&self) -> bool {
        self.shared.ages
    }

    /// The devices, for everything but reading them: their names, their
    /// lights, a rescan.
    pub fn devices(&self) -> MutexGuard<'_, Devices> {
        lock(&self.shared.devices)
    }

    /// What the loop waits on for input: the pipe, or the devices.
    #[must_use]
    pub fn raw_fds(&self) -> Vec<RawFd> {
        match &self.woken {
            Some(pipe) => vec![pipe.as_raw_fd()],
            None => self.devices().raw_fds().collect(),
        }
    }

    /// What arrived since the last call: what the thread queued, or what
    /// the devices `ready` names have waiting (every device for `None`).
    pub fn take(&self, ready: Option<&[RawFd]>) -> Vec<Taken> {
        if let Some(pipe) = &self.woken {
            drain(pipe);
            return std::mem::take(&mut *lock(&self.shared.queue));
        }
        let locked = lock(&self.shared.lane).is_some();
        let mut devices = self.devices();
        devices.set_per_report(locked);
        let read = match ready {
            Some(fds) => devices.read_stamped(fds),
            None => {
                let all: Vec<RawFd> = devices.raw_fds().collect();
                devices.read_stamped(&all)
            }
        };
        read.into_iter()
            .map(|(input, at)| Taken {
                input,
                at,
                forwarded: false,
            })
            .collect()
    }

    /// The age lines the thread has said since the last call.
    #[must_use]
    pub fn said(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.shared.said))
    }

    /// Where a locked pointer's motion goes from now on; `None` while no
    /// lock is held.
    pub fn set_lane(&self, lane: Option<Lane>) {
        *lock(&self.shared.lane) = lane;
    }

    /// What a connection's slot keeps to call [`LaneHandle::sync`] with.
    #[must_use]
    pub fn handle(&self) -> LaneHandle {
        LaneHandle(Arc::clone(&self.shared))
    }

    /// [`LaneHandle::sync`].
    pub fn sync_lane(&self, serial: u64, pointers: &[ObjectId]) {
        self.handle().sync(serial, pointers);
    }
}

/// The lane, as a connection's slot holds it.
#[derive(Debug, Clone)]
pub struct LaneHandle(Arc<Shared>);

impl LaneHandle {
    /// Before the connection `serial` is sent what its client queued: a
    /// relative pointer the client has destroyed since the lane was set
    /// leaves the lane first (the module's comment says why).
    pub fn sync(&self, serial: u64, pointers: &[ObjectId]) {
        let mut lane = lock(&self.0.lane);
        if let Some(held) = lane.as_mut()
            && held.serial == serial
            && held.pointers != pointers
        {
            held.pointers.retain(|id| pointers.contains(id));
            if held.pointers.is_empty() {
                *lane = None;
            }
        }
    }
}

impl Drop for Front {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread: wait on the devices, read what is ready, write a locked
/// pointer's motion and queue the rest.
fn run(shared: &Shared) {
    while !shared.stop.load(Ordering::Relaxed) {
        let fds: Vec<RawFd> = lock(&shared.devices).raw_fds().collect();
        let ready = match crate::wait::wait(&fds, &[], Some(LOOK_AGAIN)) {
            Ok(ready) if !ready.is_empty() => ready,
            Ok(_) => continue,
            Err(_) => {
                std::thread::sleep(LOOK_AGAIN);
                continue;
            }
        };
        // The lane's lock is held from the read to the last write, so the
        // loop cannot take a destroyed pointer out of the lane between the
        // two (the module's comment).
        let lane = lock(&shared.lane);
        let read: Vec<Stamped> = {
            let mut devices = lock(&shared.devices);
            devices.set_per_report(lane.is_some());
            devices.read_stamped(&ready)
        };
        let mut taken = Vec::with_capacity(read.len());
        let mut said = Vec::new();
        for (input, at) in read {
            let forwarded = match (&input, lane.as_ref()) {
                (Input::Motion { dx, dy }, Some(lane)) => {
                    let micros = if at == 0 { monotonic_us() } else { at };
                    let bytes = compositor_server::relative_motion_message(
                        &lane.pointers,
                        micros,
                        *dx,
                        *dy,
                    );
                    // A client that cannot take it is the loop's to find
                    // gone; the motion still goes to the seat.
                    let sent = lane.sender.send(&bytes).is_ok();
                    if sent && shared.ages && at != 0 {
                        said.push(age_line(at, monotonic_us(), "lane"));
                    }
                    sent
                }
                _ => false,
            };
            taken.push(Taken {
                input,
                at,
                forwarded,
            });
        }
        drop(lane);
        if !said.is_empty() {
            lock(&shared.said).extend(said);
        }
        if !taken.is_empty() {
            lock(&shared.queue).extend(taken);
            if let Some(wake) = &shared.wake {
                poke(wake);
            }
        }
    }
}

/// A non-blocking pipe: its reading end and its writing end.
pub(crate) fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut ends = [0; 2];
    #[expect(
        unsafe_code,
        reason = "AUDIT: pipe2 writes two descriptors into a local array of two"
    )]
    // SAFETY: `ends` is two writable ints, which is what pipe2 fills.
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if made != 0 {
        return None;
    }
    Some((own(ends[0]), own(ends[1])))
}

/// Own a descriptor `pipe2` just made.
fn own(raw: RawFd) -> OwnedFd {
    #[expect(
        unsafe_code,
        reason = "AUDIT: a descriptor pipe2 just made is this process's and owned by nothing else"
    )]
    // SAFETY: `raw` was made by `pipe2` in [`pipe`] and is owned only here.
    unsafe {
        OwnedFd::from_raw_fd(raw)
    }
}

/// Wake the loop: one byte down the pipe. A full pipe has a wake in it
/// already.
pub(crate) fn poke(wake: &OwnedFd) {
    let byte = [1u8];
    #[expect(
        unsafe_code,
        reason = "AUDIT: write of one byte from a local into a pipe this process owns"
    )]
    // SAFETY: the buffer is one readable byte, and the descriptor is open
    // for as long as `wake` lives.
    let _ = unsafe { libc::write(wake.as_raw_fd(), byte.as_ptr().cast(), 1) };
}

/// Empty the pipe, which is only ever wakes.
pub(crate) fn drain(pipe: &OwnedFd) {
    let mut bytes = [0u8; 64];
    loop {
        #[expect(
            unsafe_code,
            reason = "AUDIT: read into a local buffer of its own length from a pipe this process owns"
        )]
        // SAFETY: the buffer is `bytes.len()` writable bytes, and the
        // descriptor is open for as long as `pipe` lives.
        let read = unsafe { libc::read(pipe.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if read <= 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests;
