//! `/bin/splash`: the boot splash on `/dev/dri/card0`, and `splash --quit`,
//! which asks it to leave (`docs/SPLASH.md`).
//!
//! `splash.service` starts it beside `devmgr`, before `/` is known (init's
//! `early.target`). It waits for the card,
//! shows the picture the loader left on the firmware's screen, and from then
//! on reads the kernel log, `devmgr-events` and the keyboards:
//!
//! * the slash rises as init reaches its targets;
//! * a stage that runs long, a driver started again and a boot that stopped
//!   each get one line under the mark;
//! * Esc shows the boot log and Esc again the mark; Enter restarts a boot
//!   that stopped.
//!
//! The compositor needs the card to itself: it is master only if nobody is
//! when it opens the card (`docs/DISPLAY.md` §2.5). So `hyprix.service` runs
//! `splash --quit` first, which connects to the splash's socket and waits
//! for the connection to end. The splash lights the whole slash, exits, and
//! the end of its process is the end of both the connection and its hold on
//! the card. A `--quit` that finds nobody listening leaves a file, which a
//! splash that had not bound its socket yet reads before it opens the card.

#[cfg(target_os = "linux")]
mod program {
    use std::fs::File;
    use std::io::{self, Read, Write as _};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use compositor_drm::{Card, Dumb, Plan, plan};
    use compositor_evecho::{Device, event_nodes};
    use compositor_splash::boot::{Boot, Note};
    use compositor_splash::picture::{self, Frame};
    use compositor_splash::{LEFT, SHOWN, SOCKET};
    use ferrix_linux_abi::input::{EV_KEY, Event, KEY_ESC};
    use ferrix_splash::LOADED;

    /// `KEY_ENTER`.
    const KEY_ENTER: u16 = 28;
    /// The devmgr-events socket, without its leading NUL.
    const EVENTS: &[u8] = b"ferrix.devmgr.events";
    /// Milliseconds between looks at the clock: a note counts seconds.
    const TICK_MS: libc::c_int = 250;
    /// Milliseconds between tries for something not there yet: the events
    /// socket, a keyboard.
    const RETRY_MS: u64 = 500;
    /// Milliseconds between tries for the card: the screen shows nothing of
    /// ours from its driver's start until this finds it.
    const CARD_MS: libc::c_int = 50;

    /// Set by `SIGTERM`: init is stopping the unit.
    static STOP: AtomicBool = AtomicBool::new(false);

    /// The `SIGTERM` handler.
    extern "C" fn stop(_signal: libc::c_int) {
        STOP.store(true, Ordering::Release);
    }

    /// An address in the abstract namespace.
    fn abstract_address(name: &[u8]) -> io::Result<SocketAddr> {
        SocketAddr::from_abstract_name(name)
    }

    /// `splash --quit`: ask a running splash to leave and wait until it has;
    /// with none, say so to one that may be starting.
    pub(super) fn quit() {
        let connected =
            abstract_address(SOCKET).and_then(|address| UnixStream::connect_addr(&address));
        match connected {
            Ok(mut stream) => {
                // The splash never writes: the read ends when its process
                // does, and the card is closed with it.
                let _ = stream.read(&mut [0_u8; 1]);
                let _ = File::create(SHOWN);
            }
            Err(_) => {
                let _ = File::create(LEFT);
            }
        }
    }

    /// The screen: the card, and the one buffer it shows.
    struct Screen {
        /// The card, master while this is the only root open.
        card: Card,
        /// The buffer on it.
        dumb: Dumb,
        /// Its size in pixels.
        size: (u32, u32),
    }

    impl Screen {
        /// Open the card and put an ink buffer on its first screen.
        fn open() -> io::Result<Self> {
            let card = Card::open()?;
            let plan: Plan = plan(&card)?;
            let size = plan.size();
            let dumb = Dumb::new(&card, size.0, size.1)?;
            let mut screen = Self { card, dumb, size };
            // The loader's picture, in the buffer before the card shows it
            // and told to the card again once it does.
            screen.draw(|frame| picture::splash(frame, LOADED, None));
            let _ = screen.card.set_mode(&plan, &screen.dumb)?;
            screen.draw(|frame| picture::splash(frame, LOADED, None));
            // The one line it says: when the screen became its own, for
            // whoever times a boot from the log.
            let _ = writeln!(io::stdout(), "splash: card0 {}x{}", size.0, size.1);
            Ok(screen)
        }

        /// Draw into the buffer and have the card show it.
        fn draw(&mut self, picture: impl FnOnce(&mut Frame<'_>)) {
            let (width, height) = self.size;
            let pitch = self.dumb.pitch;
            if let Some(mut frame) = Frame::new(self.dumb.pixels(), width, height, pitch) {
                picture(&mut frame);
            }
            // A card that scans the buffer out itself needs no telling and
            // may refuse it; one that shows a copy takes this as the frame.
            let _ = self.card.dirty(&self.dumb, &[(0, 0, width, height)]);
        }
    }

    /// What the screen shows: redrawn when it differs from what should be.
    #[derive(PartialEq)]
    struct Shown {
        /// The slash's level.
        level: u32,
        /// The line under the mark.
        note: Option<Note>,
        /// Whether the log is up, and how many records it had.
        log: Option<u64>,
    }

    /// Everything the splash reads.
    struct Sources {
        /// Where `--quit` connects.
        listener: UnixListener,
        /// `/dev/kmsg`, until a read fails.
        kmsg: Option<File>,
        /// `devmgr-events`, once it is there.
        events: Option<UnixStream>,
        /// Bytes of an event line not yet ended.
        partial: Vec<u8>,
        /// The keyboards.
        keys: Vec<Device>,
        /// When what was missing was last tried for.
        tried: u64,
        /// `--quit`'s connection, kept until the process ends: its end is
        /// the answer.
        leaving: Option<UnixStream>,
    }

    impl Sources {
        /// Bind the socket and open what is there.
        fn open() -> io::Result<Self> {
            let listener = UnixListener::bind_addr(&abstract_address(SOCKET)?)?;
            listener.set_nonblocking(true)?;
            let kmsg = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open("/dev/kmsg")
                .ok();
            let mut sources = Self {
                listener,
                kmsg,
                events: None,
                partial: Vec::new(),
                keys: Vec::new(),
                tried: 0,
                leaving: None,
            };
            sources.retry(u64::MAX);
            Ok(sources)
        }

        /// Try again for the events socket and the keyboards, if they are
        /// missing and it is time.
        fn retry(&mut self, now: u64) {
            if now.saturating_sub(self.tried) < RETRY_MS {
                return;
            }
            self.tried = now;
            if self.events.is_none() {
                self.events = abstract_address(EVENTS)
                    .and_then(|address| UnixStream::connect_addr(&address))
                    .and_then(|stream| stream.set_nonblocking(true).map(|()| stream))
                    .ok();
            }
            if self.keys.is_empty() {
                self.keys = event_nodes()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|node| Device::open(node).ok())
                    .filter(|device| device.reports(EV_KEY))
                    .collect();
            }
        }

        /// Whether `--quit` has connected or init sent `SIGTERM`. The
        /// connection is kept until the process ends.
        fn asked_to_leave(&mut self) -> bool {
            if self.leaving.is_none()
                && let Ok((stream, _)) = self.listener.accept()
            {
                self.leaving = Some(stream);
            }
            self.leaving.is_some() || STOP.load(Ordering::Acquire)
        }

        /// Wait until something can be read, or a tick has passed.
        fn wait(&self, timeout: libc::c_int) {
            let fds = [self.listener.as_raw_fd()]
                .into_iter()
                .chain(self.kmsg.as_ref().map(AsRawFd::as_raw_fd))
                .chain(self.events.as_ref().map(AsRawFd::as_raw_fd))
                .chain(self.keys.iter().map(Device::raw_fd));
            let mut polled: Vec<libc::pollfd> = fds
                .map(|fd: RawFd| libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            let count = libc::nfds_t::try_from(polled.len()).unwrap_or(0);
            // SAFETY: `polled` is `count` live pollfds; a signal that ends
            // the call early is what the caller looks for next.
            let _ = unsafe { libc::poll(polled.as_mut_ptr(), count, timeout) };
        }

        /// Hand the kernel log's new records to `boot`.
        fn read_log(&mut self, boot: &mut Boot, now: u64) {
            let mut record = [0_u8; 4096];
            while let Some(kmsg) = self.kmsg.as_mut() {
                match kmsg.read(&mut record) {
                    Ok(0) => return,
                    Ok(read) => {
                        let bytes = record.get(..read).unwrap_or_default();
                        boot.record(&String::from_utf8_lossy(bytes), now);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                    // Records were overwritten before they were read: the
                    // next read is the oldest one left.
                    Err(error) if error.raw_os_error() == Some(libc::EPIPE) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => self.kmsg = None,
                }
            }
        }

        /// Hand devmgr's new events to `boot`.
        fn read_events(&mut self, boot: &mut Boot, now: u64) {
            let mut bytes = [0_u8; 1024];
            while let Some(events) = self.events.as_mut() {
                match events.read(&mut bytes) {
                    Ok(0) => self.events = None,
                    Ok(read) => self
                        .partial
                        .extend_from_slice(bytes.get(..read).unwrap_or_default()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => self.events = None,
                }
            }
            while let Some(end) = self.partial.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.partial.drain(..=end).collect();
                boot.driver_event(&String::from_utf8_lossy(&line), now);
            }
        }

        /// The keys pressed since the last call.
        fn pressed(&mut self) -> Vec<u16> {
            let mut events: Vec<Event> = Vec::new();
            // A keyboard that has gone is dropped; `retry` finds the next.
            self.keys
                .retain(|device| device.read_events(&mut events).is_ok());
            events
                .iter()
                .filter(|event| event.r#type == EV_KEY && event.value == 1)
                .map(|event| event.code)
                .collect()
        }
    }

    /// Wait for the card, unless the compositor is starting already.
    fn screen(sources: &mut Sources) -> Option<Screen> {
        loop {
            if sources.asked_to_leave() || std::fs::exists(LEFT).unwrap_or(false) {
                return None;
            }
            if let Ok(screen) = Screen::open() {
                return Some(screen);
            }
            // SAFETY: no descriptors are passed, so this only waits, and a
            // signal ends it early.
            let _ = unsafe { libc::poll(core::ptr::null_mut(), 0, CARD_MS) };
        }
    }

    /// The splash, from its start to the compositor's.
    pub(super) fn run() -> io::Result<()> {
        // SAFETY: `stop` only stores to an atomic, which a handler may.
        let _ = unsafe { libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t) };
        let mut sources = Sources::open()?;
        let Some(mut screen) = screen(&mut sources) else {
            return Ok(());
        };
        let started = Instant::now();
        let now = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut boot = Boot::new(0);
        let mut log = false;
        let mut restarting = false;
        let mut shown = None;
        while !sources.asked_to_leave() {
            let at = now();
            sources.retry(at);
            sources.read_log(&mut boot, at);
            sources.read_events(&mut boot, at);
            for key in sources.pressed() {
                if key == KEY_ESC {
                    log = !log;
                } else if key == KEY_ENTER && boot.failed() && !restarting {
                    // Init's ordered restart; the picture stays until it
                    // stops this unit.
                    restarting = std::process::Command::new("/bin/svc")
                        .arg("reboot")
                        .spawn()
                        .is_ok();
                }
            }
            let wanted = Shown {
                level: boot.level(),
                note: boot.note(at),
                log: log.then(|| boot.seen()),
            };
            if shown.as_ref() != Some(&wanted) {
                let (rows, _) = picture::log_size(screen.size.0, screen.size.1);
                screen.draw(|frame| match wanted.log {
                    Some(_) => picture::log(frame, wanted.level, boot.newest(rows)),
                    None => picture::splash(frame, wanted.level, wanted.note.as_ref()),
                });
                shown = Some(wanted);
            }
            sources.wait(TICK_MS);
        }
        // The compositor is next: the slash is whole when the screen goes
        // to it.
        boot.finish();
        screen.draw(|frame| picture::splash(frame, boot.level(), None));
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--quit") {
        program::quit();
        return std::process::ExitCode::SUCCESS;
    }
    match program::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "splash: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Only Linux, and Ferrix through its Linux ABI, have `/dev/dri`.
#[cfg(not(target_os = "linux"))]
fn main() {}
