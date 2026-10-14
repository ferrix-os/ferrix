//! The HTTP server `netboot` and `test-netboot` run: GET and HEAD of a
//! fixed set of files held in memory, under one path prefix, and nothing
//! else.
//!
//! As small as iPXE needs. HTTP/1.1 with persistent connections, because
//! iPXE keeps one open across fetches; `Content-Length` on every answer and
//! no chunking, no ranges, no compression. A thread a connection, which is
//! one or two at a time for one machine booting. Every request is logged
//! as it is answered, with what it cost.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::REPORT;
use crate::{Error, Result};

/// The longest request head read before the connection is dropped.
const MAX_HEAD: usize = 16 * 1024;
/// How long a connection may say nothing before it is closed.
const IDLE: Duration = Duration::from_secs(60);
/// How often the accepting thread looks at the stop flag.
const POLL: Duration = Duration::from_millis(20);
/// What a report is answered with: a body iPXE takes as a small image.
const REPORT_BODY: &[u8] = b"ok\n";

/// One request, as answered.
#[derive(Debug, Clone)]
pub(super) struct Request {
    /// The path asked for, without a query.
    pub(super) path: String,
    /// The status answered.
    pub(super) status: u16,
}

/// What the accepting thread and each connection share.
#[derive(Debug)]
struct State {
    files: BTreeMap<String, Vec<u8>>,
    prefix: Mutex<String>,
    requests: Mutex<Vec<Request>>,
    stop: AtomicBool,
    started: Instant,
}

/// A running server.
#[derive(Debug)]
pub(super) struct Server {
    address: SocketAddr,
    state: Arc<State>,
    accepting: Option<JoinHandle<()>>,
}

impl Server {
    /// Listen on `address` and serve `files` under `prefix`.
    pub(super) fn start(
        address: SocketAddr,
        prefix: &str,
        files: BTreeMap<String, Vec<u8>>,
    ) -> Result<Self> {
        let listener = TcpListener::bind(address)
            .map_err(|error| Error::new(format!("netboot: listening on {address}: {error}")))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let state = Arc::new(State {
            files,
            prefix: Mutex::new(prefix.to_owned()),
            requests: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            started: Instant::now(),
        });
        let shared = Arc::clone(&state);
        let accepting = std::thread::Builder::new()
            .name("netboot-http".to_owned())
            .spawn(move || accept(&listener, &shared))
            .map_err(|error| Error::new(format!("netboot: starting the server: {error}")))?;
        Ok(Server {
            address,
            state,
            accepting: Some(accepting),
        })
    }

    /// Where it listens.
    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Serve the same files under another prefix from now on.
    pub(super) fn serve_under(&self, prefix: &str) {
        prefix.clone_into(
            &mut self
                .state
                .prefix
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    /// Every request answered so far.
    pub(super) fn requests(&self) -> Vec<Request> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Serve until the process is stopped.
    pub(super) fn wait(mut self) {
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.join();
        }
    }

    /// Stop, and answer every request that was answered.
    pub(super) fn stop(self) -> Vec<Request> {
        let requests = self.requests();
        drop(self);
        requests
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.join();
        }
    }
}

/// Accept connections until told to stop, a thread each.
fn accept(listener: &TcpListener, state: &Arc<State>) {
    while !state.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let shared = Arc::clone(state);
                let spawned = std::thread::Builder::new()
                    .name("netboot-conn".to_owned())
                    .spawn(move || {
                        if let Err(error) = connection(stream, peer, &shared) {
                            println!("  netboot: {peer}: {error}");
                        }
                    });
                if let Err(error) = spawned {
                    println!("  netboot: {peer}: no thread for the connection: {error}");
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(error) => {
                println!("  netboot: accepting: {error}");
                std::thread::sleep(POLL);
            }
        }
    }
}

/// Answer requests on one connection until either end closes it.
fn connection(mut stream: TcpStream, peer: SocketAddr, state: &State) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IDLE))?;
    stream.set_write_timeout(Some(IDLE))?;
    stream.set_nodelay(true)?;
    let mut pending = Vec::new();
    while let Some(head) = read_head(&mut stream, &mut pending)? {
        let request = parse(&head);
        if !answer(&mut stream, peer, state, &request)? {
            break;
        }
    }
    Ok(())
}

/// The next request's head, up to its blank line, or `None` when the
/// client closed the connection or went quiet between requests.
fn read_head(stream: &mut TcpStream, pending: &mut Vec<u8>) -> std::io::Result<Option<Vec<u8>>> {
    loop {
        if let Some(end) = pending.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = pending.split_off(end + 4);
            return Ok(Some(std::mem::replace(pending, rest)));
        }
        if pending.len() > MAX_HEAD {
            return Err(std::io::Error::other("a request head over 16 KiB"));
        }
        let mut buffer = [0_u8; 4096];
        let read = match stream.read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if pending.is_empty()
                    && matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            return if pending.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::other("closed in the middle of a request"))
            };
        }
        pending.extend_from_slice(buffer.get(..read).unwrap_or_default());
    }
}

/// A request's method, path and whether the client keeps the connection.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Head {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) keep: bool,
}

/// Read a request head. What cannot be read becomes a method nothing
/// answers but 400.
pub(super) fn parse(head: &[u8]) -> Head {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_owned();
    let target = first.next().unwrap_or_default();
    let version = first.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default().to_owned();
    let mut keep = version == "HTTP/1.1";
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("connection")
        {
            keep = !value.trim().eq_ignore_ascii_case("close");
        }
    }
    Head { method, path, keep }
}

/// Answer one request and log it. Says whether to keep the connection.
fn answer(
    stream: &mut TcpStream,
    peer: SocketAddr,
    state: &State,
    request: &Head,
) -> std::io::Result<bool> {
    let prefix = state
        .prefix
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let name = request.path.strip_prefix(prefix.as_str());
    let report = name.and_then(|name| name.strip_prefix(REPORT));
    let (status, body): (u16, &[u8]) = match (request.method.as_str(), name, report) {
        ("GET" | "HEAD", _, Some(_)) => (200, REPORT_BODY),
        ("GET" | "HEAD", Some(name), None) => match state.files.get(name) {
            Some(file) => (200, file.as_slice()),
            None => (404, b"not found\n"),
        },
        ("GET" | "HEAD", None, None) => (404, b"not found\n"),
        ("", _, _) => (400, b"bad request\n"),
        _ => (405, b"only GET and HEAD\n"),
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let kind = if request.path.ends_with(".ipxe") || status != 200 || report.is_some() {
        "text/plain"
    } else {
        "application/octet-stream"
    };
    let keep = request.keep && status != 400;
    let started = Instant::now();
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: {kind}\r\n\
         Cache-Control: no-store\r\nConnection: {}\r\n\r\n",
        body.len(),
        if keep { "keep-alive" } else { "close" }
    );
    stream.write_all(header.as_bytes())?;
    if request.method != "HEAD" {
        stream.write_all(body)?;
    }
    stream.flush()?;
    log(
        peer,
        state,
        request,
        status,
        body.len(),
        started.elapsed(),
        report,
    );
    state
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Request {
            path: request.path.clone(),
            status,
        });
    Ok(keep)
}

/// One line for one request: when, who, what, the answer and its cost; a
/// PCI report as what it says.
fn log(
    peer: SocketAddr,
    state: &State,
    request: &Head,
    status: u16,
    bytes: usize,
    took: Duration,
    report: Option<&str>,
) {
    let at = state.started.elapsed().as_secs_f64();
    if let Some(found) = report.and_then(|report| report.strip_prefix("pci/")) {
        let mut parts = found.split('/');
        let location = parts.next().unwrap_or_default();
        let id = parts.next().unwrap_or_default();
        let class = parts.next().unwrap_or_default();
        let what = match class.get(..2) {
            Some("01") => "  <- storage",
            Some("02") => "  <- network",
            Some("03") => "  <- display",
            _ => "",
        };
        println!("  netboot: {at:8.1}s {peer} PCI {location} {id} class {class}{what}");
        return;
    }
    let seconds = took.as_secs_f64();
    let rate = if seconds > 0.0 && bytes > 64 * 1024 {
        format!(", {:.1} MB/s", bytes as f64 / seconds / 1e6)
    } else {
        String::new()
    };
    println!(
        "  netboot: {at:8.1}s {peer} {} {} {status} {} KiB in {:.2}s{rate}",
        request.method,
        request.path,
        bytes.div_ceil(1024),
        seconds
    );
}
