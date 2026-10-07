//! The networking exit criterion, as programs with expected output.
//!
//! "busybox configures `eth0`, `route` and `netstat` report through
//! `/proc/net`, a name resolves, and a file comes back byte for byte."
//!
//! # What runs where
//!
//! The guest is a real boot with a virtio-net device: the ring-3 driver in
//! `src/user/system/native/drivers/net/virtio-net`, the net ring, `src/lib/network/net`'s stack, and busybox on top of it.
//! The other end of the wire is [`crate::gateway`], and the servers the guest
//! talks to are in this file, in threads of this process, bound to the host's
//! loopback on ports the kernel chose. The gateway maps `10.0.2.2` to that
//! loopback, exactly as slirp does, so the guest reaches them by opening the
//! address its default route already points at.
//!
//! Nothing here touches the real network. That is deliberate: the test must
//! say the same thing on a laptop in a tunnel as on a build machine, and a
//! DNS answer that depends on the host's resolver is a test that fails for a
//! reason that is nobody's fault. The gateway's forwarder is pointed at
//! [`Servers::dns`] for the run.
//!
//! # What is proved
//!
//! * `ip` configures an address and a route over rtnetlink, and `ip`, `route`
//!   and `netstat` read them back through `/proc/net`.
//! * ICMP reaches the gateway and comes back.
//! * A name resolves through `/etc/resolv.conf`, which is the C library's
//!   resolver talking to `10.0.2.3:53` over UDP.
//! * A file fetched by name over HTTP arrives byte for byte: the guest's
//!   `cksum` is compared with this process's, over a body long enough to need
//!   many segments, a growing window and real acknowledgements.
//! * A UDP datagram goes out and its answer comes back.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::vfs::{Command, Expect};
use crate::{Error, Result};

#[cfg(test)]
mod tests;

/// The line the short body holds, and what the guest must print.
///
/// Served with a newline after it. Without one the guest's `wget` leaves the
/// cursor mid-line and the kernel's next line continues it, which is a
/// passing fetch that reads as a failure.
const HELLO_BODY: &str = "ferrix: fetched over TCP through the gateway";

/// The path that serves it.
const HELLO_PATH: &str = "/hello";

/// The path that serves [`big_body`].
const BIG_PATH: &str = "/big";

/// A body long enough to time: see [`SPEED_BYTES`].
const SPEED_PATH: &str = "/speed";

/// How long the body at [`SPEED_PATH`] is.
const SPEED_BYTES: usize = 32 * 1024 * 1024;

/// Where the repository [`make_repository`] writes is served from.
const GIT_REPO_PATH: &str = "/repo.git";

/// The one file in it, and what is in that file.
const GIT_FILE: &str = "greeting";

/// What is in it, which the guest prints after cloning.
const GIT_CONTENT: &str = "hello from ferrix";

/// Its one commit's subject, which the guest prints after cloning.
const GIT_SUBJECT: &str = "first commit";

/// The author and committer date of that commit, so the repository this
/// writes is the same on every host and in every run.
const GIT_DATE: &str = "2026-01-01T00:00:00+00:00";

/// How long the body at [`BIG_PATH`] is.
///
/// Long enough that the transfer is many segments rather than one, so that a
/// congestion window that never grows, an acknowledgement that never arrives
/// and a receive buffer that never drains all show up as a hang; short enough
/// that it crosses a TCG guest's ring in a second or two.
const BIG_BYTES: usize = 256 * 1024;

/// The name the DNS stub answers, and the only one it knows.
const NAME: &str = "ferrix.test";

/// What the stub answers it with: the gateway, which is the host's loopback,
/// so a fetch by name and a fetch by address reach the same server.
const NAME_ANSWER: Ipv4Addr = crate::gateway::GATEWAY_IP;

/// How long a stub thread blocks on its socket before looking at the stop
/// flag. Short enough to shut down promptly, long enough not to spin.
const TURN: Duration = Duration::from_millis(100);

/// The longest request line and headers the HTTP stub reads.
const MAX_REQUEST: usize = 8192;

/// The longest datagram the stubs take.
const MAX_DATAGRAM: usize = 1500;

/// What the UDP echo puts in front of what it was sent.
const UDP_PREFIX: &str = "ferrix-udp: ";

/// The bodies the guest fetches, made the same way on both sides.
///
/// Printable, line-broken text rather than random bytes, so that a failing
/// transfer shows up in a log as text that stops rather than as binary; and
/// generated rather than stored, because a fixture file that has to be the
/// same in two places is a fixture file that stops being the same.
fn big_body() -> Vec<u8> {
    let mut body = Vec::with_capacity(BIG_BYTES + 64);
    let mut line = 0_u32;
    while body.len() < BIG_BYTES {
        body.extend_from_slice(format!("ferrix net line {line:08} ").as_bytes());
        body.extend(std::iter::repeat_n(b'.', 40));
        body.push(b'\n');
        line = line.wrapping_add(1);
    }
    body.truncate(BIG_BYTES);
    body
}

/// POSIX `cksum`: a CRC-32 over the bytes and then over the length, one's
/// complemented.
///
/// Written out here rather than pulled in, because `cksum` is the one digest
/// this busybox and this build tool can both compute with nothing added to
/// either, and because it is twenty lines. The tests check it against the
/// values the host's own `cksum` prints.
fn cksum(bytes: &[u8]) -> u32 {
    /// The polynomial POSIX names, most significant bit first.
    const POLYNOMIAL: u32 = 0x04C1_1DB7;
    let mut crc = 0_u32;
    let mut feed = |byte: u8| {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ POLYNOMIAL
            };
        }
    };
    for &byte in bytes {
        feed(byte);
    }
    // The length follows the bytes, low byte first, and a length of zero
    // contributes nothing at all.
    let mut length = bytes.len();
    while length != 0 {
        feed(u8::try_from(length & 0xFF).unwrap_or(0));
        length >>= 8;
    }
    !crc
}

/// The servers the guest talks to, and the threads serving them.
///
/// Dropping it stops every thread. The QEMU process must be gone, or at least
/// past its last request, before that happens.
#[derive(Debug)]
pub(crate) struct Servers {
    /// Where the HTTP stub listens.
    http: SocketAddrV4,
    /// Where the DNS stub listens.
    dns: SocketAddrV4,
    /// Where the UDP echo listens.
    udp: SocketAddrV4,
    /// Set to stop every thread at the end of its next turn.
    stop: Arc<AtomicBool>,
    /// The threads, joined by [`Servers::drop`].
    threads: Vec<JoinHandle<()>>,
}

impl Servers {
    /// Bind all three on the host's loopback and start serving.
    ///
    /// # Errors
    ///
    /// A socket that cannot be bound, or a thread that cannot be started.
    pub(crate) fn start() -> Result<Servers> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| Error::new(format!("could not bind the HTTP stub: {error}")))?;
        listener.set_nonblocking(true)?;
        let http = local_v4(listener.local_addr()?)?;

        let resolver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| Error::new(format!("could not bind the DNS stub: {error}")))?;
        resolver.set_read_timeout(Some(TURN))?;
        let dns = local_v4(resolver.local_addr()?)?;

        let echo = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| Error::new(format!("could not bind the UDP echo: {error}")))?;
        echo.set_read_timeout(Some(TURN))?;
        let udp = local_v4(echo.local_addr()?)?;

        // The repository the guest clones over HTTP, written before anything
        // can ask for it. It costs a few files and one process per run, and
        // having it always means the HTTP stub answers the same paths whether
        // or not this run tests git.
        let repository = make_repository()?;

        let stop = Arc::new(AtomicBool::new(false));
        let threads = vec![
            spawn("ferrix-net-http", &stop, move |signal| {
                serve_http(&listener, signal, &repository);
            })?,
            spawn("ferrix-net-dns", &stop, move |signal| {
                serve_datagrams(&resolver, signal, answer_query);
            })?,
            spawn("ferrix-net-echo", &stop, move |signal| {
                serve_datagrams(&echo, signal, |sent, out| {
                    out.extend_from_slice(UDP_PREFIX.as_bytes());
                    out.extend_from_slice(sent);
                    true
                });
            })?,
        ];
        Ok(Servers {
            http,
            dns,
            udp,
            stop,
            threads,
        })
    }

    /// Where the gateway's `10.0.2.3:53` must forward to for the run.
    pub(crate) fn dns(&self) -> SocketAddrV4 {
        self.dns
    }

    /// A line describing what is listening, for the run's report.
    pub(crate) fn describe(&self) -> String {
        format!(
            "serving http on {}, dns on {}, udp echo on {}, all through 10.0.2.2",
            self.http.port(),
            self.dns.port(),
            self.udp.port()
        )
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// An address that must be IPv4, because both stubs were bound to one.
fn local_v4(address: std::net::SocketAddr) -> Result<SocketAddrV4> {
    match address {
        std::net::SocketAddr::V4(v4) => Ok(v4),
        std::net::SocketAddr::V6(_) => Err(Error::new(
            "a socket bound to 127.0.0.1 answered with an IPv6 address",
        )),
    }
}

/// Start a thread that runs `body` with the stop flag.
fn spawn(
    name: &str,
    stop: &Arc<AtomicBool>,
    body: impl FnOnce(&AtomicBool) + Send + 'static,
) -> Result<JoinHandle<()>> {
    let signal = Arc::clone(stop);
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || body(&signal))
        .map_err(|error| Error::new(format!("could not start the {name} thread: {error}")))
}

/// Accept connections until asked to stop, answering each in turn.
///
/// One at a time, because the guest fetches one at a time, and a stub that
/// forked a thread per connection would be a stub with a shutdown problem.
fn serve_http(listener: &TcpListener, stop: &AtomicBool, repository: &Path) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => answer_http(stream, repository),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(TURN);
            }
            Err(_) => return,
        }
    }
}

/// Read one request and write its answer.
fn answer_http(mut stream: TcpStream, repository: &Path) {
    // Blocking, whatever the listener is. The listener is non-blocking so the
    // accept loop can look at its stop flag, and on Windows a socket `accept`
    // returns inherits that; Linux's does not. Left non-blocking, the first
    // read here answers `WouldBlock` whenever the guest's request has not
    // arrived yet, the loop below takes that for the end of it, and the
    // guest's `wget` is told 404 for a request nobody read.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    // Byte at a time to the end of the headers: the request is short, and
    // this way nothing of a following request is ever consumed.
    while request.len() < MAX_REQUEST && !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => request.push(byte[0]),
            _ => break,
        }
    }
    let body = match path_of(&request) {
        Some(path) if path == HELLO_PATH => Some(format!("{HELLO_BODY}\n").into_bytes()),
        Some(path) if path == BIG_PATH => Some(big_body()),
        Some(path) if path == SPEED_PATH => Some(vec![b'x'; SPEED_BYTES]),
        // Anything else may be a file of the repository below `/repo.git`,
        // which git asks for one at a time over its dumb HTTP protocol.
        Some(path) => repository_file(repository, &path),
        None => None,
    };
    let Some(body) = body else {
        let _ = stream.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    let head = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

/// The bytes of `path` inside the served repository, or `None`.
///
/// Only paths under [`GIT_REPO_PATH`] are served, and only names made of the
/// characters git asks for, so a request can never name a file outside the
/// directory: no component is `..`, and the whole of what is left is joined
/// beneath it. A query string is cut off first, which is what makes git's
/// probe for the smart protocol -- `info/refs?service=git-upload-pack` --
/// arrive as `info/refs`, the file the dumb protocol reads.
fn repository_file(repository: &Path, path: &str) -> Option<Vec<u8>> {
    let rest = path.split('?').next()?.strip_prefix(GIT_REPO_PATH)?;
    let rest = rest.strip_prefix('/')?;
    let mut file = repository.to_path_buf();
    for part in rest.split('/') {
        let named = !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
        if !named {
            return None;
        }
        file.push(part);
    }
    std::fs::read(file).ok()
}

/// Make the repository the guest clones, under the workspace's `build`.
///
/// The host's own git writes it, since it is the far end of the conversation
/// and not what is being tested: a work tree with one file and one commit, a
/// bare clone of that, and `update-server-info`, which writes the two files
/// -- `info/refs` and `objects/info/packs` -- that let a plain file server
/// stand in for a git server. Author and committer are fixed, so the same
/// commit comes out on every host and the subject the guest prints is the
/// subject this made.
fn make_repository() -> Result<PathBuf> {
    // Once for the process, however many runs it starts: the repository is
    // the same bytes every time and nothing writes to it afterwards, and two
    // runs at once -- which the unit tests do -- would otherwise take turns
    // deleting the directory the other was serving.
    static MADE: std::sync::OnceLock<std::result::Result<PathBuf, String>> =
        std::sync::OnceLock::new();
    match MADE.get_or_init(|| write_repository().map_err(|error| error.to_string())) {
        Ok(path) => Ok(path.clone()),
        Err(message) => Err(Error::new(message.clone())),
    }
}

/// Write it, as [`make_repository`] describes.
fn write_repository() -> Result<PathBuf> {
    let directory = crate::paths::workspace_root().join("build").join("net-git");
    let _ = std::fs::remove_dir_all(&directory);
    let work = directory.join("work");
    let bare = directory.join("repo.git");
    std::fs::create_dir_all(&work)
        .map_err(|error| Error::new(format!("making {}: {error}", work.display())))?;
    std::fs::write(work.join(GIT_FILE), format!("{GIT_CONTENT}\n"))
        .map_err(|error| Error::new(format!("writing {GIT_FILE}: {error}")))?;
    let git = |arguments: &[&str]| -> Result<()> {
        let mut command = std::process::Command::new("git");
        let _ = command
            .args(arguments)
            .current_dir(&directory)
            .env("GIT_AUTHOR_NAME", "ferrix")
            .env("GIT_AUTHOR_EMAIL", "ferrix@ferrix.test")
            .env("GIT_AUTHOR_DATE", GIT_DATE)
            .env("GIT_COMMITTER_NAME", "ferrix")
            .env("GIT_COMMITTER_EMAIL", "ferrix@ferrix.test")
            .env("GIT_COMMITTER_DATE", GIT_DATE)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        let output = command
            .output()
            .map_err(|error| Error::new(format!("running git {}: {error}", arguments.join(" "))))?;
        if output.status.success() {
            return Ok(());
        }
        Err(Error::new(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    };
    git(&["-C", "work", "init", "-q", "-b", "main"])?;
    git(&["-C", "work", "add", GIT_FILE])?;
    git(&["-C", "work", "commit", "-q", "-m", GIT_SUBJECT])?;
    git(&["clone", "-q", "--bare", "work", "repo.git"])?;
    git(&["-C", "repo.git", "update-server-info"])?;
    Ok(bare)
}

/// The path out of a request's first line, `GET <path> HTTP/1.x`.
fn path_of(request: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(request).ok()?;
    let first = text.lines().next()?;
    let mut fields = first.split(' ');
    let method = fields.next()?;
    let path = fields.next()?;
    (method == "GET").then(|| path.to_owned())
}

/// Answer datagrams until asked to stop.
///
/// `answer` fills `out` and says whether to send it; a datagram it refuses is
/// dropped, which is what a resolver does with a question it cannot parse.
///
/// Each turn waits with a peek and only then receives, as the gateway does
/// ([`crate::gateway::next_frame`]): on Windows a receive whose read timeout
/// fires as a datagram arrives can lose that datagram, and a stub that loses
/// a query leaves its asker waiting out its own timeout. With [`TURN`] cut to
/// 1 ms, this DNS stub lost 18 of 20,000 queries receiving directly and none
/// of 20,000 peeking first, on a 24-thread Windows PC (2026-10-07); CI's Windows
/// runner lost the one DNS query of `the_stubs_serve_what_the_commands_expect`
/// to it on 2026-10-05 and 2026-10-07.
fn serve_datagrams(
    socket: &UdpSocket,
    stop: &AtomicBool,
    mut answer: impl FnMut(&[u8], &mut Vec<u8>) -> bool,
) {
    let mut buffer = [0_u8; MAX_DATAGRAM];
    let mut out = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let Ok((read, from)) = crate::gateway::next_frame(socket, &mut buffer) else {
            continue;
        };
        out.clear();
        if answer(buffer.get(..read).unwrap_or_default(), &mut out) {
            let _ = socket.send_to(&out, from);
        }
    }
}

/// Answer a DNS query for [`NAME`] with [`NAME_ANSWER`].
///
/// Only an A query for that one name, and only the first question: this is a
/// fixed answer for a fixed test, not a resolver. Anything else gets
/// `NXDOMAIN`, so a guest that asked the wrong question sees a failure rather
/// than the right address by accident.
fn answer_query(query: &[u8], out: &mut Vec<u8>) -> bool {
    /// Bytes of a DNS header.
    const HEADER: usize = 12;
    /// A record: an IPv4 address.
    const TYPE_A: u16 = 1;
    /// The internet class.
    const CLASS_IN: u16 = 1;
    /// How long the answer may be cached, in seconds.
    const TTL: u32 = 60;

    let Some(header) = query.get(..HEADER) else {
        return false;
    };
    let questions =
        u16::from_be_bytes([*header.get(4).unwrap_or(&0), *header.get(5).unwrap_or(&0)]);
    if questions != 1 {
        return false;
    }
    let Some((name, after)) = read_name(query, HEADER) else {
        return false;
    };
    let Some(kind) = query.get(after..after + 4) else {
        return false;
    };
    let wanted = u16::from_be_bytes([*kind.first().unwrap_or(&0), *kind.get(1).unwrap_or(&0)]);
    let class = u16::from_be_bytes([*kind.get(2).unwrap_or(&0), *kind.get(3).unwrap_or(&0)]);
    // The name existing and the record existing are two different answers,
    // and a resolver needs both: `getaddrinfo` asks for A and AAAA together,
    // and `NXDOMAIN` to the AAAA question means the name does not exist at
    // all, which fails the lookup that the A answer would have satisfied.
    let found = name.eq_ignore_ascii_case(NAME);
    let known = found && wanted == TYPE_A && class == CLASS_IN;

    out.extend_from_slice(header.get(..2).unwrap_or_default());
    // QR, AA and recursion available; NXDOMAIN only for a name nobody has.
    out.extend_from_slice(&[0x85, if found { 0x80 } else { 0x83 }]);
    out.extend_from_slice(&1_u16.to_be_bytes());
    out.extend_from_slice(&u16::from(known).to_be_bytes());
    out.extend_from_slice(&0_u16.to_be_bytes());
    out.extend_from_slice(&0_u16.to_be_bytes());
    out.extend_from_slice(query.get(HEADER..after + 4).unwrap_or_default());
    if known {
        // The name again, as a pointer to the question's copy of it.
        out.extend_from_slice(&[0xC0, 0x0C]);
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        out.extend_from_slice(&4_u16.to_be_bytes());
        out.extend_from_slice(&NAME_ANSWER.octets());
    }
    true
}

/// The name at `at`, dotted, and where it ends. Labels only: a question with a
/// compression pointer in it is one nothing sends.
fn read_name(query: &[u8], at: usize) -> Option<(String, usize)> {
    /// The longest name this stub reads, which is the protocol's limit.
    const MAX_NAME: usize = 255;
    let mut name = String::new();
    let mut at = at;
    loop {
        let length = usize::from(*query.get(at)?);
        at += 1;
        if length == 0 {
            return Some((name, at));
        }
        if length > 63 || name.len() + length > MAX_NAME {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(query.get(at..at + length)?).ok()?);
        at += length;
    }
}

/// The guest's programs, in the order the kernel runs them.
///
/// The arguments hold ports the host's kernel chose a moment ago, so the
/// strings are made here and leaked: the list is built once per run, read
/// until the run ends, and lives as long as the process does anyway.
///
/// With `curl`, the image carries the curl built against ferrousli, and it
/// fetches the same two files `wget` does. With `git`, git makes a commit and
/// clones it back over HTTP.
pub(crate) fn commands(servers: &Servers, curl: bool, git: bool) -> Vec<Command> {
    let http = servers.http.port();
    let udp = servers.udp.port();
    let digest = cksum(&big_body());

    let mut commands = vec![
        // The interface the ring-3 driver brought up, configured by DHCP as a
        // distribution configures one: `udhcpc` asks over packet sockets, and
        // the image's `default.script` applies the lease over rtnetlink with
        // `ip`. Its line names what the gateway offered.
        Command {
            argv: &["ip", "link", "set", "eth0", "up"],
            status: 0,
            expect: Expect::Nothing,
        },
        Command {
            argv: &["udhcpc", "-i", "eth0", "-n", "-q", "-t", "5", "-T", "2"],
            status: 0,
            expect: Expect::Shaped(&["eth0: 10.0.2.15/24 by DHCP, router 10.0.2.2, DNS 10.0.2.3"]),
        },
        // Read back, by the three programs that read three different files.
        Command {
            argv: &["ip", "-o", "addr", "show", "eth0"],
            status: 0,
            expect: Expect::Shaped(&["*inet 10.0.2.15/24*"]),
        },
        Command {
            argv: &["route", "-n"],
            status: 0,
            expect: Expect::Shaped(&["0.0.0.0*10.0.2.2*eth0"]),
        },
        Command {
            argv: &["netstat", "-rn"],
            status: 0,
            expect: Expect::Shaped(&["0.0.0.0*10.0.2.2*eth0"]),
        },
        // ICMP to the gateway and back, which is the first thing anybody
        // tries and the shortest round trip there is.
        Command {
            argv: &["ping", "-c", "2", "-W", "5", "10.0.2.2"],
            status: 0,
            expect: Expect::Shaped(&["2 packets transmitted, 2 packets received*"]),
        },
        // And IPv6 over the loopback, by the raw ICMPv6 socket `ping6` opens:
        // the hop limit it prints came in a control message, and a stack that
        // sent none would print -1.
        Command {
            argv: &["ping6", "-c", "2", "-W", "5", "::1"],
            status: 0,
            expect: Expect::Shaped(&[
                "# bytes from ::1: seq=0 ttl=64 time=*",
                "2 packets transmitted, 2 packets received*",
            ]),
        },
        // A name, through the C library's resolver and the gateway's
        // forwarder, over UDP.
        Command {
            argv: &["nslookup", NAME, "10.0.2.3"],
            status: 0,
            expect: Expect::Shaped(&["*10.0.2.2*"]),
        },
        // A file by name: `/etc/resolv.conf`, the resolver, a connect and a
        // fetch, with nothing on the command line but the name.
        Command {
            argv: leak_argv(vec![
                "wget".to_owned(),
                "-q".to_owned(),
                "-O".to_owned(),
                "-".to_owned(),
                format!("http://{NAME}:{http}{HELLO_PATH}"),
            ]),
            status: 0,
            expect: Expect::Lines(&[HELLO_BODY]),
        },
        // And byte for byte: a quarter of a megabyte, summed on both sides.
        Command {
            argv: leak_argv(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("wget -q -O - http://10.0.2.2:{http}{BIG_PATH} | cksum"),
            ]),
            status: 0,
            expect: Expect::Lines(leak_argv(vec![format!("{digest} {BIG_BYTES}")])),
        },
        speed_command(http),
        // A datagram out and its answer back, which TCP's success does not
        // imply: the two take different paths through the stack.
        Command {
            argv: leak_argv(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("echo ferrix | nc -u -w 3 10.0.2.2 {udp}"),
            ]),
            status: 0,
            expect: Expect::Lines(leak_argv(vec![format!("{UDP_PREFIX}ferrix")])),
        },
        // What the connections left behind: `/proc/net/tcp` and the neighbour
        // table, which `arp` reads out of `/proc/net/arp`.
        // What `ifconfig` counts with, which has to name the interface and
        // have moved by now.
        // Through `sed`, because every line of this file is indented and the
        // boot log tells a kernel line from a program's by exactly that.
        Command {
            argv: &["sh", "-c", "sed 's/^ *//' /proc/net/dev; exit 5"],
            status: 5,
            expect: Expect::Shaped(&["eth0:*"]),
        },
        Command {
            argv: &["sh", "-c", "arp -n; exit 7"],
            status: 7,
            expect: Expect::Shaped(&["* (10.0.2.2) at *on eth0"]),
        },
        socket_owner_command(),
    ];
    if curl {
        commands.extend(curl_commands(http, digest));
    }
    if git {
        commands.push(git_command(http));
    }
    commands
}

/// How fast: the guest's own clock around a download it throws away.
fn speed_command(http: u16) -> Command {
    Command {
        argv: leak_argv(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!(
                "a=$(cut -d' ' -f1 /proc/uptime); \
                 wget -q -O /dev/null http://10.0.2.2:{http}{SPEED_PATH}; \
                 b=$(cut -d' ' -f1 /proc/uptime); echo \"speed {SPEED_BYTES} bytes $a $b\""
            ),
        ]),
        status: 0,
        expect: Expect::Shaped(&["speed *"]),
    }
}

/// Who owns a connection, as `lsof` and Steam's client ask it: the
/// inode a listener's row in `/proc/net/tcp` names is the one its
/// process's descriptor links to, and the uid is the one it runs as,
/// `ferrix` (1000) and not root, so that a row printing 0 for everyone
/// fails. `su` and `sh -c exec` keep the pid `$!` names. busybox's `nc`
/// listens on `::`, so the row may be in either table. Port 45679 is
/// `B26F`.
const fn socket_owner_command() -> Command {
    Command {
        argv: &[
            "sh",
            "-c",
            "su -s /bin/sh ferrix -c 'exec nc -l -p 45679' & p=$!; sleep 1; \
             set -- $(awk '$2 ~ /:B26F$/ { print $8, $10 }' /proc/net/tcp /proc/net/tcp6); \
             if [ \"$1\" = 1000 ] && ls -l /proc/$p/fd | grep -q \"socket:\\[$2\\]\"; \
             then echo tcp-row-names-the-fd-and-its-user; else echo \"row: uid $1 inode $2\"; fi; \
             kill $p",
        ],
        status: 0,
        expect: Expect::Lines(&["tcp-row-names-the-fd-and-its-user"]),
    }
}

/// git's command in [`commands`]: [`git_program`], and the four lines it
/// prints -- the file and the subject out of the local clone, and the same
/// two out of the one fetched over HTTP.
fn git_command(http: u16) -> Command {
    Command {
        argv: leak_argv(vec!["sh".to_owned(), "-c".to_owned(), git_program(http)]),
        status: 0,
        expect: Expect::Lines(&[GIT_CONTENT, GIT_SUBJECT, GIT_CONTENT, GIT_SUBJECT]),
    }
}

/// git's program in [`commands`], in two halves.
///
/// First git's own: a repository made in the guest, a file added and
/// committed, and a bare clone of it made over the local transport, which
/// runs `git-upload-pack` as a program of its own -- so the object database,
/// the index, the commit and one of git's two transports are all the guest's.
///
/// `GIT_PAGER=cat` because the guest's console is a terminal, and git
/// sends `log` through a pager on one: without it the subject arrives
/// wrapped in the cursor movements busybox's pager drew it with.
/// It prints the file and the subject out of that clone.
///
/// Then the network's: the same two lines out of a clone of the repository
/// this process serves at [`GIT_REPO_PATH`], fetched over HTTP through the
/// gateway by `git-remote-http` and the libcurl built against ferrousli.
///
/// The far end is served here rather than in the guest because neither
/// busybox on the image has the `httpd` applet -- Alpine builds it into
/// `busybox-extras`, and the musl busybox the gates run is Alpine's, which
/// cannot be reconfigured from this tree.
fn git_program(http: u16) -> String {
    format!(
        "set -e; export HOME=/tmp GIT_AUTHOR_NAME=ferrix GIT_AUTHOR_EMAIL=ferrix@ferrix.test \
         GIT_COMMITTER_NAME=ferrix GIT_COMMITTER_EMAIL=ferrix@ferrix.test GIT_PAGER=cat; \
         cd /tmp; git init -q -b main work; cd work; \
         echo '{GIT_CONTENT}' > {GIT_FILE}; git add {GIT_FILE}; git commit -q -m '{GIT_SUBJECT}'; \
         git clone -q --bare /tmp/work /tmp/bare.git; \
         git clone -q /tmp/bare.git /tmp/local; \
         cat /tmp/local/{GIT_FILE}; git -C /tmp/local log --format=%s; \
         git clone -q http://10.0.2.2:{http}{GIT_REPO_PATH} /tmp/fetched; \
         cat /tmp/fetched/{GIT_FILE}; git -C /tmp/fetched log --format=%s"
    )
}

/// curl's half of [`commands`]: a file by name and a file byte for byte, as
/// `wget` fetches them, through a C library, a resolver and a TLS-capable
/// HTTP stack that are not busybox's.
///
/// Each through `sh -c`, because the kernel starts a command's `argv[0]` as a
/// busybox applet, and curl is a program of its own that the shell finds in
/// `/bin`.
fn curl_commands(http: u16, digest: u32) -> Vec<Command> {
    vec![
        Command {
            argv: leak_argv(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("curl -sS http://{NAME}:{http}{HELLO_PATH}"),
            ]),
            status: 0,
            expect: Expect::Lines(&[HELLO_BODY]),
        },
        Command {
            argv: leak_argv(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("curl -sS http://10.0.2.2:{http}{BIG_PATH} | cksum"),
            ]),
            status: 0,
            expect: Expect::Lines(leak_argv(vec![format!("{digest} {BIG_BYTES}")])),
        },
        // HTTPS, inside the guest: Mbed TLS's test server on the loopback
        // with its certificate for `localhost`, and curl trusting its test
        // CA. The certificates' validity starts in 2023 and 2019, so this
        // passes only on a guest whose clock came from firmware; a guest at
        // 1970 is refused with curl's 60. The second fetch trusts only the
        // image's Mozilla bundle, which must refuse the test CA.
        Command {
            argv: &["sh", "-c", HTTPS_PROGRAM],
            status: 0,
            expect: Expect::Lines(&["verified 1", "untrusted 60"]),
        },
    ]
}

/// [`curl_commands`]' HTTPS program.
const HTTPS_PROGRAM: &str = "T=/usr/share/ferrix/tls-test; /usr/libexec/ferrix/ssl_server2 server_port=4433 crt_file=$T/server5.crt key_file=$T/server5.key >/dev/null 2>&1 & sleep 3; echo verified $(curl -sS --cacert $T/test-ca2.crt https://localhost:4433/ | grep -c 'Successful connection'); curl -sS -o /dev/null https://localhost:4433/ 2>/dev/null; echo untrusted $?; kill $!";

/// Leak owned strings into the `'static` slice a [`Command`] holds, for its
/// arguments and for the lines it must print.
fn leak_argv(argv: Vec<String>) -> &'static [&'static str] {
    let leaked: Vec<&'static str> = argv
        .into_iter()
        .map(|arg| -> &'static str { Box::leak(arg.into_boxed_str()) })
        .collect();
    Box::leak(leaked.into_boxed_slice())
}
