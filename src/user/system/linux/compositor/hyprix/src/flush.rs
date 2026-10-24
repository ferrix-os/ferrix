//! A screen's flushes on a thread of their own.
//!
//! On a card that shows a copy of the frame -- a virtio-gpu, whose host
//! holds it -- the frame is shown by `DIRTYFB`, and the call returns once
//! the host has shown it: under QEMU's GL window at the window's next
//! repaint, under VNC after the whole screen was read back. That took 5 to
//! 15 ms a frame, and the compositor's loop waited for it, so it answered
//! no client and drew no frame meanwhile. Linux's compositors do not wait:
//! they commit without blocking and hear of the flip by an event.
//!
//! Here the loop hands the flush to this thread and goes on. At most one
//! flush waits behind the one under way; a frame drawn while one waits is
//! merged into it, its rectangles added, so the card is never more than one
//! frame behind and the newest frame is the one shown. The order the card
//! sees is safe: the GPU's drawing and the flush go to the device on one
//! queue, which the host serves in order and each command whole, so a
//! flush that goes after the next frame's drawing shows that frame entire,
//! never a mix.
//!
//! A card whose driver has gone answers `ENODEV`; the thread says so
//! ([`FlushThread::gone`]) and stops, and the screen is lost as it would
//! have been at the call.
//!
//! # Flips
//!
//! Every other frame a card is shown -- a page flip to one of the screen's
//! own buffers or to a client's, a `DIRTYFB` of a buffer the host copies --
//! goes to a [`FlipThread`]. On Ferrix the kernel answers `PAGE_FLIP` and
//! `DIRTYFB` only once the driver has shown the frame, up to five seconds
//! (`src/kernel/src/interfaces/display/drm.rs`): the Intel driver copies the
//! frame in pipe A's vertical blank and answers then, 7 to 17 ms later, and
//! a stuck one not at all until the kernel gives up with `ETIMEDOUT`. Made
//! on the loop, that was the loop's time: the pointer, the keys and every
//! client waited with it, 23 % of a game's map load and runs of 1.3 s on
//! the N150, and 5 s a frame while its driver was stuck.
//!
//! Unlike a flush, a flip is never merged: the buffer it shows is not
//! drawn into again until the card has answered, as a Wayland compositor
//! waits for the flip's event. So one flip is out at a time, and the loop
//! hears of its answer through a pipe in its wait ([`FlipThread::raw_fd`]).
//! The bookkeeping that follows a flip -- which buffer is drawn into next,
//! which client's buffer the card shows -- is done when the answer is
//! taken, not when the flip is handed over.

use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

/// More rectangles than one flush carries: past it a flush is of the whole
/// screen, as `DIRTYFB` takes too many.
const MERGED_CLIPS: usize = 64;

/// A flush: the framebuffer, and what changed as `(x, y, width, height)`,
/// an empty list for all of it.
type Job = (u32, Vec<(u32, u32, u32, u32)>);

/// What the loop and the thread share.
#[derive(Debug, Default)]
struct State {
    /// The flush waiting to be made: the framebuffer and what changed, an
    /// empty list for all of it.
    waiting: Option<Job>,
    /// The thread is to end.
    stop: bool,
    /// The card's driver has gone.
    gone: bool,
    /// What the card refused, said once.
    refused: Option<String>,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// A screen's flushing thread.
#[derive(Debug)]
pub(crate) struct FlushThread {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl FlushThread {
    /// Start flushing on `flusher`.
    ///
    /// # Errors
    ///
    /// The thread could not be started.
    pub(crate) fn start(flusher: compositor_drm::Flusher) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let theirs = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("flush".to_owned())
            .spawn(move || run(&theirs, &flusher))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Show `clips` of `framebuffer`, each `(x, y, width, height)`, without
    /// waiting: merged into the flush that waits, if one does.
    pub(crate) fn flush(&self, framebuffer: u32, clips: &[(u32, u32, u32, u32)]) {
        let mut state = self.lock();
        match state.waiting.as_mut() {
            Some((waiting, merged)) if *waiting == framebuffer => {
                if !merged.is_empty() {
                    merged.extend_from_slice(clips);
                    if merged.len() > MERGED_CLIPS || clips.is_empty() {
                        merged.clear();
                    }
                }
            }
            _ => state.waiting = Some((framebuffer, clips.to_vec())),
        }
        drop(state);
        self.shared.wake.notify_one();
    }

    /// Whether the card's driver went away under a flush.
    pub(crate) fn gone(&self) -> bool {
        self.lock().gone
    }

    /// What the card refused since this was last asked, if anything.
    pub(crate) fn refused(&self) -> Option<String> {
        self.lock().refused.take()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for FlushThread {
    fn drop(&mut self) {
        self.lock().stop = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread: each waiting flush in turn, until told to stop or the card
/// goes.
fn run(shared: &Shared, flusher: &compositor_drm::Flusher) {
    loop {
        let (framebuffer, clips) = {
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if state.stop {
                    return;
                }
                if let Some(job) = state.waiting.take() {
                    break job;
                }
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        if let Err(error) = flusher.dirty(framebuffer, &clips) {
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            if error.raw_os_error() == Some(libc::ENODEV) {
                state.gone = true;
                return;
            }
            state.refused = Some(error.to_string());
        }
    }
}

/// A frame handed to the card: the call that shows it, made on the
/// flipping thread.
pub(crate) type Flip = Box<dyn FnOnce() -> io::Result<()> + Send>;

/// How a flip went: what the card said, when the flip was handed to it and
/// when it answered.
#[derive(Debug)]
pub(crate) struct Answer {
    /// What the card said.
    pub(crate) result: io::Result<()>,
    /// When the call was made.
    pub(crate) began: Instant,
    /// When it returned: on a card that answers in the vertical blank, the
    /// blank.
    pub(crate) ended: Instant,
}

/// What the loop and the flipping thread share.
#[derive(Default)]
struct Flipping {
    /// The flip to make.
    flip: Option<Flip>,
    /// Whether the thread is in a flip now.
    running: bool,
    /// The last flip's answer, until the loop takes it.
    answer: Option<Answer>,
    /// The thread is to end.
    stop: bool,
}

struct FlipShared {
    state: Mutex<Flipping>,
    wake: Condvar,
    /// The pipe's two ends: a byte in it is an answer waiting. Both are
    /// held here, so that whichever of the loop and the thread lets go of
    /// them last closes them, and a write never finds its reader gone.
    read: OwnedFd,
    write: OwnedFd,
}

/// A screen's flipping thread: at most one flip out at a time.
pub(crate) struct FlipThread {
    shared: Arc<FlipShared>,
    thread: Option<JoinHandle<()>>,
    /// Whether a flip was handed over whose answer has not been taken.
    out: bool,
}

impl core::fmt::Debug for FlipThread {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("FlipThread")
            .field("out", &self.out)
            .finish_non_exhaustive()
    }
}

impl FlipThread {
    /// Start a flipping thread.
    ///
    /// # Errors
    ///
    /// No pipe could be made, or no thread started.
    pub(crate) fn start() -> io::Result<Self> {
        let (read, write) =
            crate::front::pipe().ok_or_else(|| io::Error::other("no pipe for the flips"))?;
        let shared = Arc::new(FlipShared {
            state: Mutex::default(),
            wake: Condvar::new(),
            read,
            write,
        });
        let theirs = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("flip".to_owned())
            .spawn(move || flipping(&theirs))?;
        Ok(Self {
            shared,
            thread: Some(thread),
            out: false,
        })
    }

    /// Hand `flip` to the thread and return at once. The caller hands over
    /// no other until it has taken this one's [`FlipThread::answer`].
    pub(crate) fn send(&mut self, flip: Flip) {
        let mut state = self.lock();
        state.flip = Some(flip);
        state.answer = None;
        drop(state);
        self.out = true;
        self.shared.wake.notify_all();
    }

    /// Whether a flip is out: handed over, its answer not yet taken.
    pub(crate) const fn out(&self) -> bool {
        self.out
    }

    /// What becomes readable when a flip's answer is waiting.
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.shared.read.as_raw_fd()
    }

    /// The flip's answer, if it has come; taken, the next may be sent.
    pub(crate) fn answer(&mut self) -> Option<Answer> {
        let answer = self.lock().answer.take();
        if answer.is_some() {
            // Written with the answer, under the same lock: emptied now,
            // the pipe wakes the wait for the next answer only.
            crate::front::drain(&self.shared.read);
            self.out = false;
        }
        answer
    }

    /// Wait for the flip that is out, and take its answer: for a change to
    /// the screen that cannot be made while a flip may still show one of
    /// its buffers. `None` when none is out.
    pub(crate) fn wait(&mut self) -> Option<Answer> {
        if !self.out {
            return None;
        }
        let mut state = self.lock();
        while state.answer.is_none() {
            state = self
                .shared
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(state);
        self.answer()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Flipping> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for FlipThread {
    /// The thread is told to end. One still in a flip is not waited for --
    /// a stuck card would hold whoever drops it for the kernel's five
    /// seconds -- and ends when the call returns.
    fn drop(&mut self) {
        let mut state = self.lock();
        state.stop = true;
        let running = state.running;
        drop(state);
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take()
            && !running
        {
            let _ = thread.join();
        }
    }
}

/// The flipping thread: each flip in turn, until told to stop.
fn flipping(shared: &FlipShared) {
    loop {
        let flip = {
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if state.stop {
                    return;
                }
                if let Some(flip) = state.flip.take() {
                    state.running = true;
                    break flip;
                }
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let began = Instant::now();
        let result = flip();
        let ended = Instant::now();
        let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.running = false;
        state.answer = Some(Answer {
            result,
            began,
            ended,
        });
        crate::front::poke(&shared.write);
        drop(state);
        shared.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A waiting flush takes in the next frame's rectangles; a flush of the
    /// whole screen stays one; and too many become the whole screen.
    #[test]
    fn a_waiting_flush_takes_in_the_next() {
        let shared = Arc::new(Shared::default());
        let thread = FlushThread {
            shared: Arc::clone(&shared),
            thread: None,
        };
        thread.flush(7, &[(0, 0, 10, 10)]);
        thread.flush(7, &[(5, 5, 10, 10)]);
        assert_eq!(
            shared.state.lock().unwrap().waiting,
            Some((7, vec![(0, 0, 10, 10), (5, 5, 10, 10)]))
        );
        thread.flush(7, &[]);
        assert_eq!(shared.state.lock().unwrap().waiting, Some((7, vec![])));
        thread.flush(7, &[(1, 1, 1, 1)]);
        assert_eq!(
            shared.state.lock().unwrap().waiting,
            Some((7, vec![])),
            "all of it stays all of it"
        );
        shared.state.lock().unwrap().waiting = None;
        for at in 0..=MERGED_CLIPS as u32 {
            thread.flush(9, &[(at, 0, 1, 1)]);
        }
        assert_eq!(shared.state.lock().unwrap().waiting, Some((9, vec![])));
        thread.flush(3, &[(0, 0, 1, 1)]);
        assert_eq!(
            shared.state.lock().unwrap().waiting,
            Some((3, vec![(0, 0, 1, 1)])),
            "another framebuffer replaces it"
        );
    }

    /// A flip is out until its answer is taken, the pipe says when it has
    /// come, and the thread goes on to the next.
    #[test]
    fn a_flip_is_answered_through_the_pipe() {
        let mut thread = FlipThread::start().expect("a flipping thread");
        assert!(thread.answer().is_none() && !thread.out());
        let (go, wait) = std::sync::mpsc::channel::<()>();
        thread.send(Box::new(move || {
            let _ = wait.recv();
            Err(io::Error::from_raw_os_error(libc::ETIMEDOUT))
        }));
        assert!(thread.out());
        assert!(thread.answer().is_none(), "answered before the card did");
        assert!(
            crate::wait::wait(&[thread.raw_fd()], &[], Some(std::time::Duration::ZERO))
                .expect("a wait")
                .is_empty(),
            "the pipe woke before the answer came"
        );
        go.send(()).expect("the flip waits");
        let ready = crate::wait::wait(
            &[thread.raw_fd()],
            &[],
            Some(std::time::Duration::from_secs(10)),
        )
        .expect("a wait");
        assert_eq!(ready, [thread.raw_fd()]);
        let answer = thread.answer().expect("the answer");
        assert_eq!(
            answer.result.err().and_then(|error| error.raw_os_error()),
            Some(libc::ETIMEDOUT)
        );
        assert!(!thread.out());
        assert!(
            crate::wait::wait(&[thread.raw_fd()], &[], Some(std::time::Duration::ZERO))
                .expect("a wait")
                .is_empty(),
            "the pipe still says an answer waits"
        );
        thread.send(Box::new(|| Ok(())));
        assert!(thread.wait().expect("the second answer").result.is_ok());
    }

    /// A thread stuck in a flip does not hold whoever lets it go.
    #[test]
    fn a_stuck_flip_is_not_waited_for() {
        let mut thread = FlipThread::start().expect("a flipping thread");
        let (_go, wait) = std::sync::mpsc::channel::<()>();
        let (started, running) = std::sync::mpsc::channel::<()>();
        thread.send(Box::new(move || {
            let _ = started.send(());
            let _ = wait.recv_timeout(std::time::Duration::from_secs(30));
            Ok(())
        }));
        running
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the flip started");
        let dropped = Instant::now();
        drop(thread);
        assert!(
            dropped.elapsed() < std::time::Duration::from_secs(5),
            "dropping waited for the stuck flip"
        );
    }
}
