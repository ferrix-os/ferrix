//! The clipboard agent: the host's selection and the guest's, made one.
//!
//! `docs/CLIPBOARD.md` §6 is the specification. This program sits between two
//! sockets and belongs to neither protocol:
//!
//! * **`\0ferrix.vport`**, the abstract name `src/user/system/native/drivers/console/vport` binds --
//!   not a path, because `devmgr` starts the driver before `/` moves onto the
//!   root volume and a path it bound would stay behind in the initramfs's
//!   `/tmp` (`docs/INIT.md` §7.3). Everything written there goes
//!   out on the virtio-serial port to QEMU's own half of vdagent, and
//!   everything the host sends arrives here. `ferrix-vdagent` is the codec;
//!   the driver carries bytes and understands none of them.
//! * **the Wayland socket**, where this is an `ext-data-control` client. That
//!   protocol and not `wl_data_device` because this program has no window: an
//!   ordinary clipboard client sets the selection with a serial from an input
//!   event it was the focus for, and an agent is never the focus.
//!   `ext-data-control` exists for exactly this and the compositor already
//!   serves it (`src/user/system/linux/compositor/hyprix/src/clipboard.rs`).
//!
//! # The two directions
//!
//! **Host to guest.** The host grabs, saying which types it has. If one of
//! them is UTF-8 text the agent asks for it, and when the bytes come it
//! becomes the guest's selection owner: a source, an offer of
//! `text/plain;charset=utf-8`, and `set_selection`. A guest program pasting
//! makes the compositor ask this source for the data, which is written down
//! the descriptor it hands over.
//!
//! **Guest to host.** A guest program copies, the compositor offers the
//! selection here, and if it carries text the agent grabs on the host's side.
//! When the host asks, the data is read out of the offer through a pipe and
//! sent. The agent ignores its own selection, or copying would loop.
//!
//! # Waiting
//!
//! Both sockets are non-blocking and the loop services each in turn with a
//! short sleep, for the reason `src/user/system/native/drivers/console/vport` polls: there is no wait that takes
//! both, and a clipboard is a human-speed thing.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::time::{Duration, Instant};

use compositor_protocol::core::{self, wl_display, wl_registry};
use compositor_protocol::ext_data_control::{
    self, ext_data_control_device_v1 as control_device,
    ext_data_control_manager_v1 as control_manager, ext_data_control_offer_v1 as control_offer,
    ext_data_control_source_v1 as control_source,
};
use compositor_socket::{Connection, RecvError};
use compositor_wire::{Arg, ArgType, Fd, Interface, ObjectId, Reader, Writer};
use ferrix_vdagent::chunk::{self, Reassembler};
use ferrix_vdagent::message::{AGENT_CAPS, ClipboardType, Message, Selection, Shape, Types};

/// The MIME type version 1 carries, which is what `src/user/system/linux/compositor/clip` offers
/// and what `ClipboardType::Utf8Text` maps to.
const TEXT: &str = "text/plain;charset=utf-8";

/// The manager this agent binds, by the name the registry advertises.
const MANAGER_NAME: &str = ext_data_control::EXT_DATA_CONTROL_MANAGER_V1.name;

/// Where `src/user/system/native/drivers/console/vport` listens (`docs/CLIPBOARD.md` §6).
const PORT_NAME: &[u8] = b"ferrix.vport";

/// How long a turn of the loop rests before looking at both sockets again.
const TICK: Duration = Duration::from_millis(10);

/// How long to keep trying the port before deciding this boot has none.
///
/// `devmgr` starts `src/user/system/native/drivers/console/vport` long before the compositor starts its
/// `exec-once` programs, so the socket is normally there already -- but a
/// race that loses the clipboard for the whole boot is not worth leaving,
/// and a boot genuinely without `--clipboard` only waits this once.
const PORT_PATIENCE: Duration = Duration::from_secs(10);

/// How long a new connection waits for the host's capabilities before it
/// says its own unprompted. QEMU says them as the port opens, so this is only
/// for a host that did so before anyone was connected to hear.
const HOST_PATIENCE: Duration = Duration::from_secs(2);

/// The longest a port that keeps closing is left alone before it is taken up
/// again: what a boot whose port never answers costs is one connection a
/// second, and a viewer that attaches is joined within a second of it.
const REOPEN_PATIENCE: Duration = Duration::from_secs(1);

/// How long a read of the guest's selection may take before it is given up
/// on. A request must be answered, so a refusal goes back rather than
/// nothing (`docs/CLIPBOARD.md` §4.3, rule 1).
const PIPE_PATIENCE: Duration = Duration::from_millis(500);

/// The largest selection carried in either direction, §7(a)'s draft answer.
const MAX_SELECTION: usize = 1024 * 1024;

/// The fixed object ids, as `src/user/system/linux/compositor/clip` numbers its own.
mod id {
    use compositor_wire::ObjectId;

    pub(super) const DISPLAY: ObjectId = ObjectId(1);
    pub(super) const REGISTRY: ObjectId = ObjectId(2);
    pub(super) const SYNC: ObjectId = ObjectId(3);
    pub(super) const MANAGER: ObjectId = ObjectId(4);
    pub(super) const SEAT: ObjectId = ObjectId(5);
    pub(super) const DEVICE: ObjectId = ObjectId(6);
    /// Sources are made and destroyed as the host grabs and releases, so they
    /// are numbered upwards from here rather than reusing one id.
    pub(super) const FIRST_SOURCE: u32 = 16;
}

fn main() {
    let _arguments =
        compositor_evecho::init::unshell(std::env::args().skip(1).collect::<Vec<String>>());
    match run() {
        Ok(()) => {}
        Err(quiet) if quiet.quiet => {
            // A boot without `--clipboard` has no port and no agent to be.
            // That is a boot without a clipboard, not a boot with an error
            // (§7(d)), so it says so once and leaves happy.
            say(&format!("vdagent: {}", quiet.text));
        }
        Err(loud) => {
            say(&format!("vdagent: failed: {}", loud.text));
            std::process::exit(1);
        }
    }
}

/// Something that stopped the agent, and whether it is worth a failure.
struct Stopped {
    text: String,
    quiet: bool,
}

impl Stopped {
    fn loud(text: impl Into<String>) -> Stopped {
        Stopped {
            text: text.into(),
            quiet: false,
        }
    }

    fn quiet(text: impl Into<String>) -> Stopped {
        Stopped {
            text: text.into(),
            quiet: true,
        }
    }
}

/// Connect to the driver's socket, trying for [`PORT_PATIENCE`].
fn connect_port() -> Result<UnixStream, Stopped> {
    let until = Instant::now() + PORT_PATIENCE;
    let name = PORT_NAME.escape_ascii();
    let address = SocketAddr::from_abstract_name(PORT_NAME)
        .map_err(|error| Stopped::loud(format!("the port's name \\0{name}: {error}")))?;
    let port = loop {
        match UnixStream::connect_addr(&address) {
            Ok(port) => break port,
            Err(error) if Instant::now() >= until => {
                return Err(Stopped::quiet(format!(
                    "no clipboard port at \\0{name} ({error}); this boot has no clipboard"
                )));
            }
            Err(_) => std::thread::sleep(TICK),
        }
    };
    port.set_nonblocking(true)
        .map_err(|error| Stopped::loud(format!("the port would not go non-blocking: {error}")))?;
    Ok(port)
}

fn run() -> Result<(), Stopped> {
    let port = connect_port()?;

    let display = std::env::var("WAYLAND_DISPLAY")
        .map_err(|_| Stopped::loud("WAYLAND_DISPLAY is not set"))?;
    let path = compositor_socket::socket_path(&display)
        .map_err(|error| Stopped::loud(format!("the compositor's socket: {error}")))?;
    let stream = UnixStream::connect(&path)
        .map_err(|error| Stopped::loud(format!("connecting to {}: {error}", path.display())))?;
    let connection = Connection::new(stream)
        .map_err(|error| Stopped::loud(format!("the connection: {error}")))?;

    let mut agent = Agent::new(connection, port);
    agent.start();
    say("vdagent: joined the host's clipboard to this one");
    agent.pump()
}

/// The port closing again and again with nothing said on it in between.
///
/// One close is the host's own doing and wants answering at once (see
/// [`Agent::reconnect`]). A second with no byte heard since the first is a
/// port nobody is behind: the driver answers only uid 0 and closes any other
/// peer as it connects (`docs/CLIPBOARD.md` §6), so an agent a desktop
/// started as its user was closed, took the port up, and was closed again,
/// every [`TICK`] for as long as the desktop ran, each time with a line on
/// the console -- 89 a second, 85,000 of them in one boot, 0.23 of a processor
/// here and as much again in the kernel writing them out. So each close of a
/// streak waits twice as long as the last, up to [`REOPEN_PATIENCE`], and a
/// streak is said twice: when it begins and when it has reached that wait.
#[derive(Debug, Default)]
struct Reopens {
    /// How many times in a row the port has closed with nothing heard.
    streak: u32,
}

/// What a close of the port asks for.
#[derive(Debug, PartialEq, Eq)]
struct Closed {
    /// How long to leave the port before taking it up again.
    wait: Duration,
    /// The line to say about it, for the closes that get one.
    line: Option<String>,
}

impl Reopens {
    /// How long the close that made the streak `streak` long waits: nothing
    /// for the first, a [`TICK`] for the second, and twice the last from
    /// there to [`REOPEN_PATIENCE`].
    fn wait(streak: u32) -> Duration {
        match streak.checked_sub(2) {
            None => Duration::ZERO,
            Some(doublings) => 1_u32
                .checked_shl(doublings)
                .and_then(|times| TICK.checked_mul(times))
                .map_or(REOPEN_PATIENCE, |wait| wait.min(REOPEN_PATIENCE)),
        }
    }

    /// The port closed.
    fn closed(&mut self) -> Closed {
        self.streak = self.streak.saturating_add(1);
        let wait = Self::wait(self.streak);
        let line = if self.streak == 1 {
            Some("vdagent: the host reopened the port; taking it up again".to_owned())
        } else if wait == REOPEN_PATIENCE && Self::wait(self.streak.saturating_sub(1)) < wait {
            Some(format!(
                "vdagent: the port has closed {} times with nothing said on it; \
                 looking again every {} ms and saying no more until it speaks",
                self.streak,
                REOPEN_PATIENCE.as_millis()
            ))
        } else {
            None
        };
        Closed { wait, line }
    }

    /// The port gave bytes, which ends a streak. The line to say if the
    /// streak was long enough to have gone quiet.
    fn heard(&mut self) -> Option<String> {
        let streak = std::mem::take(&mut self.streak);
        (streak > 1).then(|| format!("vdagent: the port spoke after closing {streak} times"))
    }
}

/// The agent: a Wayland client, a vdagent peer, and what each owes the other.
struct Agent {
    // -- Wayland --
    connection: Connection,
    out: Writer,
    globals: BTreeMap<String, (u32, u32)>,
    bound: bool,
    /// The source this agent owns, when the host's selection is being offered
    /// to the guest.
    source: Option<ObjectId>,
    next_source: u32,
    /// The offer the compositor last announced, and whether it carries text.
    offer: Option<ObjectId>,
    offer_has_text: bool,
    /// The offer that *is* the selection, once the compositor says so.
    selection: Option<ObjectId>,

    // -- vdagent --
    port: UnixStream,
    frames: Vec<u8>,
    shape: Shape,
    /// What the host gave, which the guest is offered.
    held: Option<Vec<u8>>,
    /// Whether the host says it owns the selection.
    host_owns: bool,
    /// Whether this connection has said its capabilities.
    announced: bool,
    /// The serial of the last grab either side made, which the next grab
    /// this agent makes goes past.
    grab_serial: u32,
    /// When this connection to the port was made.
    connected_at: Instant,
    /// The closes since the port last gave a byte.
    reopens: Reopens,
    /// When a port that closed is taken up again, while it is being left
    /// alone; `None` while there is a connection to read.
    closed_until: Option<Instant>,
}

impl Agent {
    fn new(connection: Connection, port: UnixStream) -> Agent {
        Agent {
            connection,
            out: Writer::new(),
            globals: BTreeMap::new(),
            bound: false,
            source: None,
            next_source: id::FIRST_SOURCE,
            offer: None,
            offer_has_text: false,
            selection: None,
            port,
            frames: Vec::new(),
            // Until the host says otherwise, assume the shape QEMU announces,
            // which is the only peer there is (§4.2).
            shape: Shape::QEMU_CLIPBOARD,
            held: None,
            host_owns: false,
            announced: false,
            connected_at: Instant::now(),
            grab_serial: 0,
            reopens: Reopens::default(),
            closed_until: None,
        }
    }

    /// Ask for the registry, and tell the host what this agent can do.
    fn start(&mut self) {
        request(
            &mut self.out,
            id::DISPLAY,
            wl_display::request::GET_REGISTRY,
            &[ArgType::NewId],
            &[Arg::NewId(id::REGISTRY)],
        );
        request(
            &mut self.out,
            id::DISPLAY,
            wl_display::request::SYNC,
            &[ArgType::NewId],
            &[Arg::NewId(id::SYNC)],
        );
        // The capabilities wait for the host's: see `announce_once`.
        self.connected_at = Instant::now();
    }

    /// Say this agent's capabilities once a connection: in answer to the
    /// host's, or unprompted with `request` 1 if the host has said nothing
    /// within [`HOST_PATIENCE`].
    ///
    /// Once and no more, because QEMU 10 resets its clipboard serial -- by
    /// closing the port -- on every capabilities message after the first
    /// (`vdagent_chr_recv_caps`, v10.2.1). Announcing unprompted and then
    /// answering the host's request as well was two, and every connection
    /// closed itself a moment after it opened.
    fn announce_once(&mut self, request: bool) {
        if self.announced {
            return;
        }
        self.announced = true;
        self.send_host(&Message::AnnounceCapabilities {
            request,
            caps: AGENT_CAPS,
        });
    }

    /// Read and answer both sockets until one of them ends.
    fn pump(&mut self) -> Result<(), Stopped> {
        loop {
            match self.connection.receive() {
                Ok(_) | Err(RecvError::WouldBlock) => {}
                Err(RecvError::Closed) => {
                    return Err(Stopped::quiet("the compositor closed the connection"));
                }
                Err(error) => return Err(Stopped::loud(format!("reading: {error:?}"))),
            }
            let (consumed, claimed) = self.read()?;
            if consumed > 0 {
                self.connection.consume(consumed, claimed);
            }
            self.take_from_host()?;
            if !self.announced
                && self.closed_until.is_none()
                && self.connected_at.elapsed() >= HOST_PATIENCE
            {
                self.announce_once(true);
            }
            self.flush()?;
            std::thread::sleep(TICK);
        }
    }

    // -- The host's side ---------------------------------------------------

    /// Take whatever the port has, and answer every whole message in it.
    fn take_from_host(&mut self) -> Result<(), Stopped> {
        // A port that keeps closing is left alone for a while ([`Reopens`]);
        // the Wayland side of the loop goes on meanwhile.
        if let Some(until) = self.closed_until {
            if Instant::now() < until {
                return Ok(());
            }
            self.reconnect()?;
        }
        let mut bytes = [0_u8; chunk::MAX_PAYLOAD * 2];
        loop {
            match self.port.read(&mut bytes) {
                Ok(0) => return self.port_closed(),
                Ok(read) => {
                    if let Some(line) = self.reopens.heard() {
                        say(&line);
                    }
                    say(&format!("vdagent: {read} bytes from the port"));
                    self.frames
                        .extend_from_slice(bytes.get(..read).unwrap_or_default());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Stopped::loud(format!("the port: {error}"))),
            }
        }
        self.reassemble()
    }

    /// The port read as closed: take it up again, at once if this is the
    /// first close since it last spoke and after [`Reopens`]'s wait if not.
    fn port_closed(&mut self) -> Result<(), Stopped> {
        let closed = self.reopens.closed();
        if let Some(line) = closed.line {
            say(&line);
        }
        if closed.wait.is_zero() {
            return self.reconnect();
        }
        // Nothing of the old conversation outlives the close, whenever the
        // new one starts.
        self.frames.clear();
        self.host_owns = false;
        self.closed_until = Instant::now().checked_add(closed.wait);
        Ok(())
    }

    /// Take the port up again after the host closed it.
    ///
    /// QEMU closes the port on purpose once the first capabilities have
    /// crossed -- "reopen the agent connection to reset the serial state",
    /// `ui/vdagent.c` -- and opens it again once the guest's end has closed
    /// and reopened, which the driver does; the driver lets go of this
    /// connection meanwhile. So a closed port is a new conversation, not an
    /// end: half a message is dropped, the host owns nothing any more, and
    /// the capabilities are said again.
    fn reconnect(&mut self) -> Result<(), Stopped> {
        self.closed_until = None;
        self.port = connect_port()?;
        self.frames.clear();
        self.shape = Shape::QEMU_CLIPBOARD;
        self.host_owns = false;
        self.announced = false;
        self.connected_at = Instant::now();
        // QEMU forgets its serials on a new connection's capabilities.
        self.grab_serial = 0;
        Ok(())
    }

    /// Cut the stream into messages and act on each.
    fn reassemble(&mut self) -> Result<(), Stopped> {
        if self.frames.is_empty() {
            return Ok(());
        }
        let stream = std::mem::take(&mut self.frames);
        let mut buffer = vec![0_u8; MAX_SELECTION + chunk::MAX_PAYLOAD];
        let mut reassembler = Reassembler::new(&mut buffer);
        let mut at = 0;
        // Where the last whole message ended. What follows it goes back to
        // wait for the next read, from its first chunk: a message longer
        // than a read -- a host paste of more than about a kilobyte -- has
        // chunks already fed that a fresh reassembler must see again, and
        // keeping only the bytes after them lost the message's start and
        // stopped the agent at the next chunk.
        let mut whole = 0;
        while at < stream.len() {
            let rest = stream.get(at..).unwrap_or_default();
            let taken = match reassembler.feed(rest) {
                Ok(taken) => taken,
                // A message bigger than this agent will hold, or a chunk that
                // is not one. Either way the stream cannot be resynchronised,
                // so the agent says so rather than carrying on with a wrong
                // idea of where it is.
                Err(error) => return Err(Stopped::loud(format!("the host's framing: {error:?}"))),
            };
            if taken == 0 {
                break;
            }
            at += taken;
            if let Some(message) = reassembler.message() {
                // A message this codec does not know is not an error -- the
                // mouse and the monitors ride the same wire.
                if let Ok(message) = Message::decode(message, self.shape) {
                    self.on_host_message(message)?;
                }
                reassembler.take();
                whole = at;
            }
        }
        self.frames = stream.get(whole..).unwrap_or_default().to_vec();
        Ok(())
    }

    fn on_host_message(&mut self, message: Message<'_>) -> Result<(), Stopped> {
        match message {
            Message::AnnounceCapabilities { request, caps } => {
                say(&format!(
                    "vdagent: the host announced caps {caps:#x} (asking back: {request})"
                ));
                self.shape = Shape::from_caps(caps);
                // Whether or not the host asked: the one announcement this
                // connection gets, and no request back, since the host's is
                // here.
                let _ = request;
                self.announce_once(false);
            }
            Message::ClipboardGrab {
                selection,
                types,
                serial,
            } => {
                if selection != Selection::Clipboard {
                    return Ok(());
                }
                if let Some(serial) = serial {
                    self.grab_serial = serial;
                }
                self.host_owns = true;
                say("vdagent: the host grabbed its clipboard");
                if types.holds(ClipboardType::Utf8Text) {
                    // By demand: a grab says only what is available, so the
                    // bytes are asked for now and arrive later (§4.2).
                    self.send_host(&Message::ClipboardRequest {
                        selection,
                        kind: ClipboardType::Utf8Text,
                    });
                }
            }
            Message::Clipboard {
                selection,
                kind,
                data,
            } => {
                if selection == Selection::Clipboard
                    && kind == ClipboardType::Utf8Text
                    && data.len() <= MAX_SELECTION
                {
                    say(&format!(
                        "vdagent: took {} bytes from the host's clipboard",
                        data.len()
                    ));
                    self.held = Some(data.to_vec());
                    self.own_the_selection();
                }
            }
            Message::ClipboardRelease { selection } => {
                if selection == Selection::Clipboard {
                    self.host_owns = false;
                    self.held = None;
                    self.drop_source();
                }
            }
            Message::ClipboardRequest { selection, kind } => {
                self.answer_host(selection, kind)?;
            }
            Message::Other { kind, data } => {
                say(&format!(
                    "vdagent: the host sent message {kind}, {} bytes",
                    data.len()
                ));
            }
        }
        Ok(())
    }

    /// Answer the host's request for the guest's selection. Always answered,
    /// with `None` when it cannot be (§4.3, rule 1).
    fn answer_host(&mut self, selection: Selection, kind: ClipboardType) -> Result<(), Stopped> {
        let text = if selection == Selection::Clipboard && kind == ClipboardType::Utf8Text {
            self.read_selection()
        } else {
            None
        };
        match text {
            Some(bytes) => self.send_host(&Message::Clipboard {
                selection,
                kind: ClipboardType::Utf8Text,
                data: &bytes,
            }),
            None => self.send_host(&Message::Clipboard {
                selection,
                kind: ClipboardType::None,
                data: &[],
            }),
        }
        Ok(())
    }

    /// Frame a message and put it on the port.
    fn send_host(&mut self, message: &Message<'_>) {
        let mut body = vec![0_u8; message.encoded_len(self.shape)];
        let Ok(written) = message.encode(self.shape, &mut body) else {
            return;
        };
        let mut framed = vec![0_u8; chunk::framed_len(written)];
        let Ok(len) = chunk::frame(body.get(..written).unwrap_or_default(), &mut framed) else {
            return;
        };
        // The port is non-blocking, but a clipboard message is small and the
        // driver's staging takes a whole chunk at a time; a short write is
        // retried rather than losing the tail, which would desynchronise the
        // host's reassembly for good.
        let mut at = 0;
        let until = Instant::now() + PIPE_PATIENCE;
        while at < len && Instant::now() < until {
            let rest = framed.get(at..len).unwrap_or_default();
            match self.port.write(rest) {
                Ok(0) => break,
                Ok(sent) => at += sent,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    // -- The guest's side --------------------------------------------------

    /// Become the guest's selection owner, offering what the host gave.
    fn own_the_selection(&mut self) {
        if !self.bound || self.held.is_none() {
            return;
        }
        self.drop_source();
        let source = ObjectId(self.next_source);
        self.next_source = self.next_source.saturating_add(1);
        request(
            &mut self.out,
            id::MANAGER,
            control_manager::request::CREATE_DATA_SOURCE,
            &[ArgType::NewId],
            &[Arg::NewId(source)],
        );
        request(
            &mut self.out,
            source,
            control_source::request::OFFER,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(TEXT))],
        );
        request(
            &mut self.out,
            id::DEVICE,
            control_device::request::SET_SELECTION,
            &[ArgType::Object { nullable: false }],
            &[Arg::Object(source)],
        );
        self.source = Some(source);
    }

    /// Give up the source, if there is one.
    fn drop_source(&mut self) {
        if let Some(source) = self.source.take() {
            request(
                &mut self.out,
                source,
                control_source::request::DESTROY,
                &[],
                &[],
            );
        }
    }

    /// Read the guest's selection through a pipe, for the host to be given.
    ///
    /// Returns `None` when there is nothing to read, when it is this agent's
    /// own selection -- answering that would send the host its own text back
    /// -- or when the program that owns it does not answer in time.
    fn read_selection(&mut self) -> Option<Vec<u8>> {
        let offer = self.selection?;
        if !self.offer_has_text || self.source.is_some() {
            return None;
        }
        let (read, write) = pipe()?;
        request(
            &mut self.out,
            offer,
            control_offer::request::RECEIVE,
            &[ArgType::Str { nullable: false }, ArgType::Fd],
            &[Arg::Str(Some(TEXT)), Arg::Fd(Fd(write))],
        );
        // The request has to reach the compositor before anything can come
        // back down the pipe, and the owning program answers on its own
        // thread of events.
        let _flushed = self.flush();
        close(write);
        let mut file = unsafe_file(read);
        let mut bytes = Vec::new();
        let until = Instant::now() + PIPE_PATIENCE;
        let mut chunk = [0_u8; 4096];
        while Instant::now() < until && bytes.len() <= MAX_SELECTION {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(chunk.get(..read).unwrap_or_default()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if bytes.is_empty() { None } else { Some(bytes) }
    }

    /// Tell the host the guest has something, so it can ask for it.
    fn grab_on_the_host(&mut self) {
        if self.source.is_some() || !self.offer_has_text {
            return;
        }
        let Ok(types) = Types::new(&[ClipboardType::Utf8Text]) else {
            return;
        };
        // A serial only exists when both sides agreed to carry one. It counts
        // on from the last grab either side made: QEMU discards a guest grab
        // whose serial is below the last it saw (`vdagent_clipboard_recv_grab`),
        // so a constant zero lost to every host copy after the first, and the
        // guest's copies stopped reaching the host.
        self.grab_serial = self.grab_serial.wrapping_add(1);
        let serial = self.shape.serial.then_some(self.grab_serial);
        self.send_host(&Message::ClipboardGrab {
            selection: Selection::Clipboard,
            serial,
            types,
        });
        say("vdagent: offered this desktop's selection to the host");
    }

    // -- The Wayland plumbing ----------------------------------------------

    fn flush(&mut self) -> Result<(), Stopped> {
        if self.out.is_empty() {
            return Ok(());
        }
        let (bytes, fds) = self.out.take();
        self.connection
            .send(&bytes, &fds)
            .map_err(|error| Stopped::loud(format!("writing: {error:?}")))
    }

    /// Decode every whole event in the buffer.
    ///
    /// Gives how many bytes whole events took and how many descriptors they
    /// claimed, which the connection must be told so it does not close one
    /// this program now owns.
    fn read(&mut self) -> Result<(usize, usize), Stopped> {
        let fds = self.connection.fds();
        let bytes = self.connection.bytes().to_vec();
        let mut reader = Reader::new(&bytes, &fds);
        while !reader.is_done() {
            let Ok(header) = reader.peek() else {
                break;
            };
            let Some(interface) = self.interface_of(header.sender) else {
                return Err(Stopped::loud(format!(
                    "an event for object {}",
                    header.sender.0
                )));
            };
            let Some(method) = interface.event(header.opcode) else {
                return Err(Stopped::loud(format!(
                    "{} has no event {}",
                    interface.name, header.opcode
                )));
            };
            let (_, args) = match reader.read(method.signature) {
                Ok(read) => read,
                Err(compositor_wire::Error::Incomplete { .. }) => break,
                Err(error) => {
                    return Err(Stopped::loud(format!(
                        "{}.{}: {error:?}",
                        interface.name, method.name
                    )));
                }
            };
            self.event(header.sender, header.opcode, &args)?;
        }
        Ok((reader.consumed(), reader.descriptors_taken()))
    }

    fn interface_of(&self, id: ObjectId) -> Option<&'static Interface> {
        if self.offer == Some(id) || self.selection == Some(id) || id.is_server() {
            return Some(&ext_data_control::EXT_DATA_CONTROL_OFFER_V1);
        }
        // A source this agent made and has since let go of is still a
        // source: the compositor may have sent it `send` or `cancelled`
        // before it read the destroy, and Wayland lets those arrive. They are
        // read and ignored -- a `send`'s descriptor closes as it is dropped
        // -- where an unknown object used to stop the agent ("an event for
        // object 16", when a terminal's copy replaced the host's selection).
        if self.source == Some(id)
            || (id::FIRST_SOURCE..self.next_source).contains(&id.0) && !id.is_server()
        {
            return Some(&ext_data_control::EXT_DATA_CONTROL_SOURCE_V1);
        }
        Some(match id {
            id::DISPLAY => &core::WL_DISPLAY,
            id::REGISTRY => &core::WL_REGISTRY,
            id::SYNC => &core::WL_CALLBACK,
            id::MANAGER => &ext_data_control::EXT_DATA_CONTROL_MANAGER_V1,
            id::SEAT => &core::WL_SEAT,
            id::DEVICE => &ext_data_control::EXT_DATA_CONTROL_DEVICE_V1,
            _ => return None,
        })
    }

    fn event(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) -> Result<(), Stopped> {
        match sender {
            id::DISPLAY if opcode == wl_display::event::ERROR => {
                let text = args.get(2).and_then(Arg::as_str).unwrap_or("");
                return Err(Stopped::loud(format!(
                    "the compositor refused this client: {text}"
                )));
            }
            id::REGISTRY if opcode == wl_registry::event::GLOBAL => {
                let (Some(name), Some(interface), Some(version)) = (
                    args.first().and_then(Arg::as_uint),
                    args.get(1).and_then(Arg::as_str),
                    args.get(2).and_then(Arg::as_uint),
                ) else {
                    return Ok(());
                };
                let _previous = self.globals.insert(interface.to_owned(), (name, version));
            }
            id::SYNC if opcode == core::wl_callback::event::DONE => self.bind()?,
            id::DEVICE if opcode == control_device::event::DATA_OFFER => {
                self.offer = args.first().and_then(Arg::as_object);
                self.offer_has_text = false;
            }
            id::DEVICE if opcode == control_device::event::SELECTION => {
                let named = args.first().and_then(Arg::as_object);
                if named.is_none_or(ObjectId::is_null) {
                    self.selection = None;
                    return Ok(());
                }
                self.selection = named;
                // A selection this agent set is its own doing and must not be
                // grabbed back on the host, or the two would echo.
                if self.source.is_none() {
                    self.grab_on_the_host();
                }
            }
            id::DEVICE if opcode == control_device::event::FINISHED => {
                return Err(Stopped::quiet("the compositor finished this device"));
            }
            source if self.source == Some(source) && opcode == control_source::event::SEND => {
                let (Some(mime), Some(fd)) = (
                    args.first().and_then(Arg::as_str),
                    args.get(1).and_then(Arg::as_fd),
                ) else {
                    return Ok(());
                };
                self.answer_guest(mime, fd);
            }
            source if self.source == Some(source) && opcode == control_source::event::CANCELLED => {
                self.drop_source();
            }
            other
                if self.offer == Some(other)
                    && opcode == control_offer::event::OFFER
                    && args.first().and_then(Arg::as_str) == Some(TEXT) =>
            {
                self.offer_has_text = true;
            }
            _ => {}
        }
        Ok(())
    }

    /// Write the host's text down the descriptor a paste is waiting on.
    fn answer_guest(&mut self, mime: &str, fd: Fd) {
        let Some(text) = self.held.clone().filter(|_| mime == TEXT) else {
            // The descriptor is this process's to close whether it is
            // answered or not; leaving it open hangs the program pasting.
            close(fd.0);
            return;
        };
        let mut file = unsafe_file(fd.0);
        let _written = file.write_all(&text);
        say(&format!("vdagent: gave {} bytes to a paste", text.len()));
    }

    /// Bind the manager and the seat, and take the device.
    fn bind(&mut self) -> Result<(), Stopped> {
        if self.bound {
            return Ok(());
        }
        self.bound = true;
        let manager = self
            .globals
            .get(MANAGER_NAME)
            .copied()
            .ok_or_else(|| Stopped::loud(format!("this compositor has no {MANAGER_NAME}")))?;
        let seat = self
            .globals
            .get("wl_seat")
            .copied()
            .ok_or_else(|| Stopped::loud("this compositor has no wl_seat"))?;
        bind_global(&mut self.out, manager, MANAGER_NAME, id::MANAGER);
        bind_global(&mut self.out, seat, "wl_seat", id::SEAT);
        request(
            &mut self.out,
            id::MANAGER,
            control_manager::request::GET_DATA_DEVICE,
            &[ArgType::NewId, ArgType::Object { nullable: false }],
            &[Arg::NewId(id::DEVICE), Arg::Object(id::SEAT)],
        );
        // The host may have grabbed before the compositor was ready, in which
        // case the text is already held and this is where it is offered.
        if self.held.is_some() {
            self.own_the_selection();
        }
        Ok(())
    }
}

/// `wl_registry.bind`, whose argument is the odd one in the protocol: an
/// interface name and version travel with the new id.
fn bind_global(out: &mut Writer, global: (u32, u32), interface: &str, to: ObjectId) {
    let (name, version) = global;
    request(
        out,
        id::REGISTRY,
        wl_registry::request::BIND,
        &[ArgType::Uint, ArgType::AnyNewId],
        &[
            Arg::Uint(name),
            Arg::AnyNewId {
                interface,
                version,
                id: to,
            },
        ],
    );
}

/// Queue one request.
fn request(
    out: &mut Writer,
    sender: ObjectId,
    opcode: u16,
    signature: &'static [ArgType],
    args: &[Arg<'_>],
) {
    let _queued = out.write(sender, opcode, signature, args);
}

/// A pipe, as `wl-paste` makes one: the read half for this program and the
/// write half for whoever is going to answer.
fn pipe() -> Option<(i32, i32)> {
    let mut ends = [0_i32; 2];
    #[expect(
        unsafe_code,
        reason = "AUDIT: pipe2 is not in std; it writes two descriptors into an array this frame \
                  owns, and nothing else is passed to it"
    )]
    // SAFETY: `ends` is two `int`s, which is what `pipe2` writes.
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
    if made != 0 {
        return None;
    }
    Some((ends[0], ends[1]))
}

/// A `File` for a descriptor this process owns.
fn unsafe_file(fd: i32) -> std::fs::File {
    #[expect(
        unsafe_code,
        reason = "AUDIT: the descriptor is this process's own -- one end of a pipe it made, or one \
                  the compositor passed to it -- and the File takes it, closing it once"
    )]
    // SAFETY: as the reason says: owned, and given away exactly once.
    unsafe {
        use std::os::fd::FromRawFd;
        std::fs::File::from_raw_fd(fd)
    }
}

/// Close a descriptor this process owns and is not going to use.
fn close(fd: i32) {
    #[expect(
        unsafe_code,
        reason = "AUDIT: closing a descriptor this process owns and has not given to a File"
    )]
    // SAFETY: the descriptor is this process's own and is not closed twice.
    let _closed = unsafe { libc::close(fd) };
}

/// Say a line on the standard output, flushed, as the compositor's other
/// programs do.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _written = writeln!(out, "{line}");
    let _flushed = out.flush();
}

#[cfg(test)]
mod tests {
    use super::{REOPEN_PATIENCE, Reopens, TICK};
    use std::time::Duration;

    /// QEMU's own close after the first capabilities is answered as it
    /// always was: at once, with its line, however often it comes, as long
    /// as the port says something in between.
    #[test]
    fn a_close_after_the_port_spoke_is_taken_up_at_once() {
        let mut reopens = Reopens::default();
        for _ in 0..3 {
            let closed = reopens.closed();
            assert_eq!(closed.wait, Duration::ZERO);
            assert_eq!(
                closed.line.as_deref(),
                Some("vdagent: the host reopened the port; taking it up again")
            );
            assert_eq!(reopens.heard(), None, "one close is not a streak");
        }
    }

    /// A port that closes every time it is taken up is asked less and less
    /// often, down to once a [`REOPEN_PATIENCE`], and said twice in all.
    #[test]
    fn a_port_that_keeps_closing_is_left_longer_each_time_and_said_twice() {
        let mut reopens = Reopens::default();
        let closes: Vec<_> = (0..2_000).map(|_| reopens.closed()).collect();
        let waits: Vec<Duration> = closes.iter().map(|closed| closed.wait).collect();
        assert_eq!(waits.first(), Some(&Duration::ZERO));
        assert_eq!(waits.get(1), Some(&TICK));
        assert_eq!(waits.get(2), TICK.checked_mul(2).as_ref());
        assert!(
            waits.windows(2).all(|pair| pair.first() <= pair.get(1)),
            "a wait shorter than the one before it"
        );
        assert!(waits.iter().all(|wait| *wait <= REOPEN_PATIENCE));
        assert_eq!(waits.last(), Some(&REOPEN_PATIENCE));
        // Two thousand closes at the old rate were twenty seconds; here
        // they are more than half an hour.
        let total: Duration = waits.iter().sum();
        assert!(total > Duration::from_secs(1_900), "{total:?}");
        let said: Vec<&str> = closes
            .iter()
            .filter_map(|closed| closed.line.as_deref())
            .collect();
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said.last().is_some_and(|line| line.contains("9 times")));
    }

    /// The first byte ends the streak, says how long it was, and the close
    /// after it is a first one again.
    #[test]
    fn a_byte_from_the_port_ends_the_streak() {
        let mut reopens = Reopens::default();
        for _ in 0..40 {
            let _ = reopens.closed();
        }
        assert_eq!(
            reopens.heard().as_deref(),
            Some("vdagent: the port spoke after closing 40 times")
        );
        assert_eq!(reopens.heard(), None);
        assert_eq!(reopens.closed().wait, Duration::ZERO);
    }

    /// No count of closes overflows the wait.
    #[test]
    fn a_very_long_streak_still_waits_the_longest_wait() {
        assert_eq!(Reopens::wait(u32::MAX), REOPEN_PATIENCE);
        assert_eq!(Reopens::wait(34), REOPEN_PATIENCE);
    }
}
