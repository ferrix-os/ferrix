//! The Linux front (`docs/LOGGING.md` §5, §11.1, step 3): the sockets Linux
//! programs log to, read as journald reads them.
//!
//! * `/dev/log`, a link to `/run/systemd/journal/dev-log`: syslog
//!   datagrams, musl's and glibc's `syslog()` and busybox `logger`;
//! * `/run/systemd/journal/socket`: systemd's native protocol, a datagram
//!   per record, a large one passed as a memfd. libsystemd's
//!   `sd_journal_send` sends to the name on an unconnected socket, which the
//!   kernel answers `EOPNOTSUPP` today (§14): a program that connects
//!   first (`lode cat --journal`) gets through;
//! * `/run/systemd/journal/stdout`: stdout streams, a header and then lines
//!   (`systemd-cat`, `lode cat`, `sd_journal_stream_fd`).
//!
//! Every datagram carries its sender's pid, uid and gid, stamped by the
//! kernel (`SO_PASSCRED`, checked against the sender); a stream carries them
//! from `SO_PEERCRED` at its `accept`. From the pid come `_COMM` and the
//! cgroup, `_SYSTEMD_CGROUP`, `_SYSTEMD_UNIT` and `_SYSTEMD_SLICE`, read
//! from `/proc` at once and kept 10 s for a sender that has exited since.
//! The sockets are made while lode is still root and left writable by all.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::ingest::Now;
use ferrix_lode_core::protocols::{self, StreamHeader};
use ferrix_lode_core::record::Record;

use crate::sys;

/// The syslog socket, and the link `/dev/log` is.
const DEV_LOG: &[u8] = b"/run/systemd/journal/dev-log";
/// The native protocol's socket.
const NATIVE: &[u8] = b"/run/systemd/journal/socket";
/// The stdout streams' socket.
const STDOUT: &[u8] = b"/run/systemd/journal/stdout";
/// The longest datagram read.
const DATAGRAM_MAX: usize = 256 * 1024;
/// The most a stream may hold of a line before it is cut.
const LINE_MAX: usize = 48 * 1024;
/// How long an attribution is kept for a pid, in microseconds.
const KEPT_US: u64 = 10_000_000;

/// Who a pid was, as `/proc` said.
struct Known {
    /// The pid.
    pid: u32,
    /// When it was read.
    at: u64,
    /// `comm`.
    comm: Option<Vec<u8>>,
    /// The cgroup's path.
    cgroup: Option<String>,
}

/// One stdout stream.
struct Stream {
    /// Its socket.
    fd: usize,
    /// What it has sent of a line so far.
    buf: Vec<u8>,
    /// Its header, once read.
    header: Option<StreamHeader>,
    /// Its sender, from `SO_PEERCRED`.
    peer: Option<(u32, u32, u32)>,
}

/// The sockets and the streams.
pub(crate) struct Front {
    /// `/dev/log`'s socket.
    dev_log: Option<usize>,
    /// The native protocol's socket.
    native: Option<usize>,
    /// The streams' listening socket.
    stdout: Option<usize>,
    /// The streams.
    streams: Vec<Stream>,
    /// Who recent pids were.
    known: Vec<Known>,
}

impl Front {
    /// Make the sockets, as root.
    pub(crate) fn open() -> Front {
        for dir in [&b"/run/systemd"[..], b"/run/systemd/journal"] {
            let _ = sys::mkdirat(sys::AT_FDCWD, dir, 0o755);
        }
        let dev_log = sys::datagram_at(DEV_LOG).ok();
        if dev_log.is_some() {
            let _ = sys::chmod(DEV_LOG, 0o666);
            let _ = sys::unlinkat(sys::AT_FDCWD, b"/dev/log");
            let _ = sys::symlink(DEV_LOG, b"/dev/log");
        }
        let native = sys::datagram_at(NATIVE).ok();
        if native.is_some() {
            let _ = sys::chmod(NATIVE, 0o666);
        }
        let stdout = sys::listen_at(STDOUT).ok();
        if stdout.is_some() {
            let _ = sys::chmod(STDOUT, 0o666);
        }
        Front {
            dev_log,
            native,
            stdout,
            streams: Vec::new(),
            known: Vec::new(),
        }
    }

    /// Every descriptor to wait on.
    pub(crate) fn fds(&self) -> Vec<usize> {
        [self.dev_log, self.native, self.stdout]
            .into_iter()
            .flatten()
            .chain(self.streams.iter().map(|stream| stream.fd))
            .collect()
    }

    /// Read what the sockets hold, up to 512 records a socket, into `out`.
    pub(crate) fn serve(&mut self, now: Now, out: &mut Vec<Record>) {
        let mut buf = alloc::vec![0_u8; DATAGRAM_MAX];
        for (fd, native) in [(self.dev_log, false), (self.native, true)] {
            let Some(fd) = fd else {
                continue;
            };
            for _ in 0..512 {
                let Some(got) = sys::receive(fd, &mut buf) else {
                    break;
                };
                let bytes = match got.passed {
                    // A record too large for a datagram, in a memfd.
                    Some(memfd) => {
                        let bytes = sys::read_fd_from(memfd, 0).unwrap_or_default();
                        sys::close(memfd);
                        bytes
                    }
                    None => Vec::from(buf.get(..got.length).unwrap_or_default()),
                };
                let record = if native {
                    protocols::native(&bytes, now.realtime_us, now.monotonic_us)
                } else {
                    Some(protocols::syslog(&bytes, now.realtime_us, now.monotonic_us))
                };
                if let Some(mut record) = record {
                    self.attribute(&mut record, got.sender, now);
                    out.push(record);
                }
            }
        }
        self.streams(now, out);
    }

    /// Accept new streams and read every stream's lines.
    fn streams(&mut self, now: Now, out: &mut Vec<Record>) {
        if let Some(listener) = self.stdout {
            while let Some(fd) = sys::accept(listener) {
                let peer = sys::peer_all(fd);
                self.streams.push(Stream {
                    fd,
                    buf: Vec::new(),
                    header: None,
                    peer,
                });
            }
        }
        let mut chunk = alloc::vec![0_u8; 16 * 1024];
        let mut kept = Vec::new();
        for mut stream in core::mem::take(&mut self.streams) {
            let mut open = true;
            loop {
                match sys::read(stream.fd, &mut chunk) {
                    Ok(0) => {
                        open = false;
                        break;
                    }
                    Ok(got) => stream
                        .buf
                        .extend_from_slice(chunk.get(..got).unwrap_or_default()),
                    Err(error) if error.0 == sys::EINTR => {}
                    Err(_) => break,
                }
            }
            if stream.header.is_none()
                && let Some((header, used)) = protocols::stream_header(&stream.buf)
            {
                stream.header = Some(header);
                let _ = stream.buf.drain(..used);
            }
            let mut lines: Vec<Vec<u8>> = Vec::new();
            if stream.header.is_some() {
                while let Some(end) = stream.buf.iter().position(|&byte| byte == b'\n') {
                    let mut line: Vec<u8> = stream.buf.drain(..=end).collect();
                    let _ = line.pop();
                    lines.push(line);
                }
                if stream.buf.len() > LINE_MAX || (!open && !stream.buf.is_empty()) {
                    lines.push(core::mem::take(&mut stream.buf));
                }
            }
            if let Some(header) = &stream.header {
                for line in lines {
                    let mut record =
                        protocols::stream_line(header, &line, now.realtime_us, now.monotonic_us);
                    self.attribute(&mut record, stream.peer, now);
                    out.push(record);
                }
            }
            if open && (stream.header.is_some() || stream.buf.len() <= 4096) {
                kept.push(stream);
            } else {
                sys::close(stream.fd);
            }
        }
        self.streams = kept;
    }

    /// The trusted fields of a record from its sender's `(pid, uid, gid)`.
    fn attribute(&mut self, record: &mut Record, sender: Option<(u32, u32, u32)>, now: Now) {
        let Some((pid, uid, gid)) = sender else {
            return;
        };
        record.push("_PID", alloc::format!("{pid}").as_bytes());
        record.push("_UID", alloc::format!("{uid}").as_bytes());
        record.push("_GID", alloc::format!("{gid}").as_bytes());
        self.known
            .retain(|known| now.monotonic_us.saturating_sub(known.at) < KEPT_US);
        let found = self.known.iter().position(|known| known.pid == pid);
        let index = if let Some(index) = found {
            index
        } else {
            let comm = sys::read_small(alloc::format!("/proc/{pid}/comm").as_bytes())
                .map(|comm| Vec::from(comm.strip_suffix(b"\n").unwrap_or(&comm)));
            let cgroup =
                sys::read_small(alloc::format!("/proc/{pid}/cgroup").as_bytes()).and_then(|text| {
                    String::from_utf8_lossy(&text)
                        .lines()
                        .find_map(|line| line.strip_prefix("0::").map(String::from))
                });
            self.known.push(Known {
                pid,
                at: now.monotonic_us,
                comm,
                cgroup,
            });
            self.known.len() - 1
        };
        let Some(known) = self.known.get(index) else {
            return;
        };
        if let Some(comm) = &known.comm {
            record.push("_COMM", comm);
        }
        if let Some(cgroup) = &known.cgroup {
            record.push("_SYSTEMD_CGROUP", cgroup.as_bytes());
            let mut parts = cgroup.rsplit('/');
            if let Some(unit) = parts
                .next()
                .filter(|unit| unit.ends_with(".service") || unit.ends_with(".scope"))
            {
                record.push("_SYSTEMD_UNIT", unit.as_bytes());
            }
            if let Some(slice) = parts.next().filter(|slice| slice.ends_with(".slice")) {
                record.push("_SYSTEMD_SLICE", slice.as_bytes());
            }
        }
    }
}
