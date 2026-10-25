//! `notifyd [--toggle-list]`: the desktop's notifications
//! (`docs/DESKTOP.md` §4.2).
//!
//! Run bare, it is the daemon: it owns `org.freedesktop.Notifications` on
//! the session bus when there is one, reads `devmgr`'s driver events from
//! `\0ferrix.devmgr.events` and the log's alerts from lode's
//! `/run/lode/alerts` (`docs/LOGGING.md` §7.4), and shows them as banners
//! at the top centre of the focused screen. `SIGUSR1` opens and closes the list of recent ones.
//! `--toggle-list` sends that signal to the running daemon, which is what
//! the top bar's clock runs when clicked.

use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use compositor_dbus::{Connection, Message, session_path};
use compositor_notifyd::bridge::{Lines, SOCKET};
use compositor_notifyd::bus::{self, NAME, closed, invoked};
use compositor_notifyd::paint::{self, Area, Hit};
use compositor_notifyd::store::{Reason, Store, from_alert_line, from_driver_line, quiet_file};
use compositor_text::Fonts;
use compositor_text::tiny_skia::{Pixmap, PixmapPaint, Transform};
use compositor_toolkit::{
    Anchor, Client, Event, Layer, LayerOptions, Margin, PointerEvent, Rect, SurfaceId, TimerId,
    WatchId,
};

/// `SIGUSR1`.
const SIGUSR1: i32 = 10;
/// How often a missing bus or event socket is tried again.
const RETRY: Duration = Duration::from_secs(5);
/// The left button.
const LEFT: u32 = 0x110;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let result = match arguments.next().as_deref() {
        None => run(),
        Some("--toggle-list") => toggle_list(),
        Some(other) => Err(format!("unknown argument {other}; notifyd [--toggle-list]")),
    };
    if let Err(error) = result {
        say(&format!("notifyd: {error}"));
        std::process::exit(1);
    }
}

/// Where the running daemon writes its pid.
fn pid_file() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("notifyd.pid")
}

fn toggle_list() -> Result<(), String> {
    let file = pid_file();
    let pid = std::fs::read_to_string(&file)
        .map_err(|error| format!("{}: {error} (is notifyd running?)", file.display()))?;
    let status = std::process::Command::new("kill")
        .args(["-USR1", pid.trim()])
        .status()
        .map_err(|error| format!("kill: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill -USR1 {} failed", pid.trim()))
    }
}

/// What is drawn and where it is.
struct Shown {
    surface: SurfaceId,
    /// The drawing, at the surface's scale.
    pixmap: Pixmap,
    /// Where clicks go.
    areas: Vec<Area>,
}

struct Daemon {
    client: Client,
    fonts: Fonts,
    store: Store,
    started: Instant,
    shown: Option<Shown>,
    list_open: bool,
    bus: Option<(Connection, WatchId)>,
    events: Option<(UnixStream, WatchId)>,
    lines: Lines,
    /// lode's alerts, and their lines.
    alerts: Option<(UnixStream, WatchId)>,
    alert_lines: Lines,
    ever_connected: bool,
    expiry: Option<TimerId>,
    retry: TimerId,
}

fn run() -> Result<(), String> {
    let mut client = Client::connect().map_err(|error| error.to_string())?;
    client
        .watch_signals(&[SIGUSR1])
        .map_err(|error| error.to_string())?;
    let retry = client.add_timer(RETRY, Some(RETRY));
    let _ = std::fs::write(pid_file(), format!("{}\n", std::process::id()));
    let mut daemon = Daemon {
        client,
        fonts: Fonts::system(),
        store: Store::default(),
        started: Instant::now(),
        shown: None,
        list_open: false,
        bus: None,
        events: None,
        lines: Lines::default(),
        alerts: None,
        alert_lines: Lines::default(),
        ever_connected: false,
        expiry: None,
        retry,
    };
    daemon.check_quiet();
    daemon.connect_bus();
    daemon.connect_events();
    daemon.connect_alerts();
    loop {
        let events = daemon
            .client
            .dispatch(None)
            .map_err(|error| error.to_string())?;
        for event in events {
            daemon.on(event);
        }
    }
}

impl Daemon {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn on(&mut self, event: Event) {
        match event {
            Event::Configure { surface, .. } | Event::Scale { surface, .. }
                if self
                    .shown
                    .as_ref()
                    .is_some_and(|shown| shown.surface == surface) =>
            {
                self.repaint();
            }
            Event::Closed(surface)
                if self
                    .shown
                    .as_ref()
                    .is_some_and(|shown| shown.surface == surface) =>
            {
                self.client.destroy(surface);
                self.shown = None;
            }
            Event::Pointer(PointerEvent::Button {
                surface,
                x,
                y,
                button: LEFT,
                pressed: true,
                ..
            }) => {
                let hit = self
                    .shown
                    .as_ref()
                    .filter(|shown| shown.surface == surface)
                    .and_then(|shown| paint::hit(&shown.areas, x, y));
                if let Some(hit) = hit {
                    self.clicked(hit);
                }
            }
            Event::Timer(timer) if Some(timer) == self.expiry => {
                self.expiry = None;
                let now = self.now_ms();
                for note in self.store.expire(now) {
                    if note.on_bus {
                        self.send(&closed(note.id, Reason::Expired));
                    }
                }
                self.changed();
            }
            Event::Timer(timer) if timer == self.retry => {
                self.check_quiet();
                if self.bus.is_none() {
                    self.connect_bus();
                }
                if self.events.is_none() {
                    self.connect_events();
                }
                if self.alerts.is_none() {
                    self.connect_alerts();
                }
            }
            Event::Signal(SIGUSR1) => {
                self.list_open = !self.list_open;
                self.changed();
            }
            Event::Readable(watch) => {
                self.check_quiet();
                if self.bus.as_ref().is_some_and(|(_, ours)| *ours == watch) {
                    self.read_bus();
                } else if self.events.as_ref().is_some_and(|(_, ours)| *ours == watch) {
                    self.read_events();
                } else if self.alerts.as_ref().is_some_and(|(_, ours)| *ours == watch) {
                    self.read_alerts();
                }
            }
            _ => {}
        }
    }

    /// Do not disturb follows its file; said when it changes.
    fn check_quiet(&mut self) {
        let config = std::env::var("XDG_CONFIG_HOME").ok();
        let home = std::env::var("HOME").ok();
        let quiet = quiet_file(config.as_deref(), home.as_deref()).is_some_and(|f| f.exists());
        if quiet != self.store.quiet() {
            self.store.set_quiet(quiet);
            say(&format!(
                "notifyd: do not disturb {}",
                if quiet { "on" } else { "off" }
            ));
        }
    }

    fn clicked(&mut self, hit: Hit) {
        match hit {
            Hit::Action(0, key) if key == "clear" => {
                self.store.clear_history();
                self.list_open = false;
            }
            Hit::Banner(id) if !self.list_open => {
                let default = self
                    .store
                    .shown()
                    .iter()
                    .find(|note| note.id == id)
                    .is_some_and(|note| {
                        note.on_bus && note.actions.iter().any(|(key, _)| key == "default")
                    });
                if default {
                    self.send(&invoked(id, "default"));
                }
                self.dismiss(id);
            }
            Hit::Close(id) if !self.list_open => self.dismiss(id),
            Hit::Action(id, key) if !self.list_open => {
                self.send(&invoked(id, &key));
                self.dismiss(id);
            }
            _ => {}
        }
        self.changed();
    }

    fn dismiss(&mut self, id: u32) {
        if let Some(note) = self.store.close(id)
            && note.on_bus
        {
            self.send(&closed(id, Reason::Dismissed));
        }
    }

    /// The store changed: draw again and set the next expiry.
    fn changed(&mut self) {
        if let Some(timer) = self.expiry.take() {
            self.client.cancel_timer(timer);
        }
        if let Some(until) = self.store.next_expiry() {
            let after = until.saturating_sub(self.now_ms());
            self.expiry = Some(self.client.add_timer(Duration::from_millis(after), None));
        }
        self.repaint();
    }

    /// Paint what should show, making, resizing or taking away the surface.
    fn repaint(&mut self) {
        let scale = self
            .shown
            .as_ref()
            .map_or(1, |shown| self.client.scale(shown.surface));
        let painted = if self.list_open {
            paint::list(&mut self.fonts, self.store.history(), scale)
        } else {
            paint::banners(&mut self.fonts, self.store.shown(), scale)
        };
        let Some((pixmap, _height, areas)) = painted else {
            if let Some(shown) = self.shown.take() {
                self.client.destroy(shown.surface);
            }
            return;
        };
        let shown = match self.shown.take() {
            Some(mut shown) => {
                shown.pixmap = pixmap;
                shown.areas = areas;
                shown
            }
            None => {
                let options = LayerOptions {
                    layer: Layer::Overlay,
                    namespace: "notifications".to_owned(),
                    size: (paint::WIDTH, paint::HEIGHT),
                    anchor: Anchor::TOP,
                    exclusive_zone: 0,
                    margin: Margin {
                        top: 4,
                        ..Margin::default()
                    },
                    ..LayerOptions::default()
                };
                match self.client.layer_surface(&options) {
                    // Drawn on its first configure.
                    Ok(surface) => {
                        self.shown = Some(Shown {
                            surface,
                            pixmap,
                            areas,
                        });
                        return;
                    }
                    Err(error) => {
                        say(&format!("notifyd: no surface: {error}"));
                        return;
                    }
                }
            }
        };
        // Clicks reach the cards and the list; between and below them they
        // fall through to the windows.
        let region: Vec<Rect> = shown
            .areas
            .iter()
            .filter(|area| matches!(area.hit, Hit::Banner(_) | Hit::List))
            .map(|area| {
                let (x, y, w, h) = area.rect;
                Rect {
                    x: x.floor() as i32,
                    y: y.floor() as i32,
                    width: w.ceil() as i32,
                    height: h.ceil() as i32,
                }
            })
            .collect();
        self.client.set_input_region(shown.surface, Some(&region));
        let pixmap = &shown.pixmap;
        let _ = self.client.draw(shown.surface, |target| {
            target.fill(compositor_text::tiny_skia::Color::TRANSPARENT);
            target.draw_pixmap(
                0,
                0,
                pixmap.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        });
        self.shown = Some(shown);
    }

    fn connect_bus(&mut self) {
        let Some(path) = std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .ok()
            .as_deref()
            .and_then(session_path)
        else {
            return;
        };
        let uid = std::fs::metadata("/proc/self").map_or(0, |meta| meta.uid());
        let mut connection = match Connection::open(&path, uid) {
            Ok(connection) => connection,
            Err(error) => {
                say(&format!("notifyd: the session bus at {path}: {error}"));
                return;
            }
        };
        match connection.request_name(NAME) {
            Ok(true) => {}
            Ok(false) => {
                say(&format!(
                    "notifyd: {NAME} is someone else's; driver events only"
                ));
                // Not tried again: the other server keeps it.
                self.client.cancel_timer(self.retry);
                self.retry = self.client.add_timer(Duration::from_secs(3600), None);
                return;
            }
            Err(error) => {
                say(&format!("notifyd: RequestName: {error}"));
                return;
            }
        }
        if connection.stream().set_nonblocking(true).is_err() {
            return;
        }
        let watch = self.client.watch_fd(connection.stream().as_raw_fd());
        say(&format!(
            "notifyd: {NAME} on the session bus as {}",
            connection.unique
        ));
        self.bus = Some((connection, watch));
        // Anything that came with RequestName's reply.
        self.read_bus();
    }

    fn read_bus(&mut self) {
        let mut changed = false;
        loop {
            let Some((connection, _)) = &mut self.bus else {
                return;
            };
            let message = match connection.receive() {
                Ok(message) => message,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    say(&format!("notifyd: the session bus went: {error}"));
                    if let Some((_, watch)) = self.bus.take() {
                        self.client.unwatch_fd(watch);
                    }
                    break;
                }
            };
            let now = self.now_ms();
            let outcome = bus::handle(&mut self.store, &message, now);
            changed |= outcome.changed;
            for reply in &outcome.send {
                self.send(reply);
            }
        }
        if changed {
            self.changed();
        }
    }

    /// Send on the bus, blocking for the write so a message is never cut.
    fn send(&mut self, message: &Message) {
        let Some((connection, _)) = &mut self.bus else {
            return;
        };
        let _ = connection.stream().set_nonblocking(false);
        let sent = connection.send(message);
        let _ = connection.stream().set_nonblocking(true);
        if let Err(error) = sent {
            say(&format!("notifyd: sending: {error}"));
        }
    }

    fn connect_events(&mut self) {
        let Ok(address) = SocketAddr::from_abstract_name(SOCKET) else {
            return;
        };
        let Ok(stream) = UnixStream::connect_addr(&address) else {
            return;
        };
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        if self.ever_connected {
            self.lines.reconnected();
        }
        self.ever_connected = true;
        let watch = self.client.watch_fd(stream.as_raw_fd());
        say("notifyd: reading devmgr's driver events");
        self.events = Some((stream, watch));
        self.read_events();
    }

    /// lode's alerts: a stream that says each alert once, as it fires.
    fn connect_alerts(&mut self) {
        let Ok(stream) = UnixStream::connect(ALERTS) else {
            return;
        };
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let watch = self.client.watch_fd(stream.as_raw_fd());
        say("notifyd: reading lode's alerts");
        self.alerts = Some((stream, watch));
    }

    fn read_alerts(&mut self) {
        let (got, gone) = match &mut self.alerts {
            Some((stream, _)) => drain(stream),
            None => return,
        };
        if gone && let Some((_, watch)) = self.alerts.take() {
            self.client.unwatch_fd(watch);
        }
        let now = self.now_ms();
        let mut changed = false;
        for line in self.alert_lines.take(&got) {
            if let Some((key, summary, body, urgency)) = from_alert_line(&line) {
                say(&format!("notifyd: {summary}: {body}"));
                let (_, pushed) = self.store.notify_keyed(&key, summary, body, urgency, now);
                for id in pushed {
                    self.send(&closed(id, Reason::Expired));
                }
                changed = true;
            }
        }
        if changed {
            self.changed();
        }
    }

    fn read_events(&mut self) {
        let mut got = Vec::new();
        let mut gone = false;
        if let Some((stream, _)) = &mut self.events {
            let mut chunk = [0_u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(count) => got.extend_from_slice(chunk.get(..count).unwrap_or_default()),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        gone = true;
                        break;
                    }
                }
            }
        }
        if gone && let Some((_, watch)) = self.events.take() {
            self.client.unwatch_fd(watch);
        }
        let now = self.now_ms();
        let mut changed = false;
        for line in self.lines.take(&got) {
            if let Some((key, summary, body, urgency)) = from_driver_line(&line) {
                say(&format!("notifyd: {summary}: {body}"));
                let (_, pushed) = self.store.notify_keyed(&key, summary, body, urgency, now);
                for id in pushed {
                    self.send(&closed(id, Reason::Expired));
                }
                changed = true;
            }
        }
        if changed {
            self.changed();
        }
    }
}

/// Where lode says its alerts.
const ALERTS: &str = "/run/lode/alerts";

/// What `stream` holds now, and whether it has closed.
fn drain(stream: &mut UnixStream) -> (Vec<u8>, bool) {
    let mut got = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return (got, true),
            Ok(count) => got.extend_from_slice(chunk.get(..count).unwrap_or_default()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return (got, false),
            Err(_) => return (got, true),
        }
    }
}

/// A line on the standard output, flushed.
fn say(line: &str) {
    let mut out = io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
