//! Units' lines handed to lode with who wrote them (`docs/LOGGING.md`
//! §4.2, §5; the wire format is `ferrix_lode_core::units`).
//!
//! init made each spawn's log pipes, so it alone knows, without asking
//! anyone, which unit, which run of it, which process and which stream a
//! line came from. It connects to lode's `/run/lode/units`, which only
//! root can reach, and writes each line there as well as to the console.
//! Nothing waits for lode: the connection is non-blocking, a line that
//! does not fit is not sent (its console copy still reaches the log, with
//! the unit read from its text), a connection that fails is dropped and
//! tried again at most once a second, and while lode is not running the
//! console is where the lines go, as before lode existed.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::logs::Said;

/// Where lode listens for init.
const SOCKET: &str = "/run/lode/units";

/// The most init holds for lode while lode is slow to read.
const PENDING_MAX: usize = 256 * 1024;

/// How often a missing lode is looked for.
const RETRY: Duration = Duration::from_secs(1);

/// The connection to lode, and what it has not taken yet.
#[derive(Debug)]
pub(crate) struct Lode {
    stream: Option<UnixStream>,
    pending: Vec<u8>,
    next_try: Instant,
    /// Lines not handed over because lode was slow; their console copies
    /// carry them.
    pub(crate) skipped: u64,
}

impl Default for Lode {
    fn default() -> Self {
        Lode {
            stream: None,
            pending: Vec::new(),
            next_try: Instant::now(),
            skipped: 0,
        }
    }
}

/// A new run's id: 128 random bits as 32 hex digits, as systemd's
/// `INVOCATION_ID`.
pub(crate) fn invocation_id() -> String {
    let mut bytes = [0_u8; 16];
    let random =
        std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if random.is_err() {
        // No randomness: the clock and a count are at least unique in a boot.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let (low, high) = bytes.split_at_mut(8);
        low.copy_from_slice(&crate::sys::monotonic().to_le_bytes());
        high.copy_from_slice(&COUNT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `said` as one line of lode's socket (`ferrix_lode_core::units::encode`):
/// `L <unit> <invocation|-> <pid> <uid> <gid> <out|err> <µs> <text>\n`.
pub(crate) fn encode(said: &Said, unit: &str, monotonic_us: u64) -> Vec<u8> {
    let whose = &said.whose;
    let mut line = format!(
        "L {unit} {} {} {} {} {} {monotonic_us} ",
        whose.invocation.as_deref().unwrap_or("-"),
        whose.pid,
        whose.ids.0,
        whose.ids.1,
        if whose.stderr { "err" } else { "out" },
    )
    .into_bytes();
    line.extend(
        said.line
            .bytes()
            .map(|byte| if byte == b'\n' { b' ' } else { byte }),
    );
    line.push(b'\n');
    line
}

/// The line that tells lode which program `pid` runs: `P <pid> <path>`.
pub(crate) fn program(pid: u32, path: &std::ffi::CStr) -> Vec<u8> {
    let mut line = format!("P {pid} ").into_bytes();
    line.extend(
        path.to_bytes()
            .iter()
            .map(|&byte| if byte == b'\n' { b' ' } else { byte }),
    );
    line.push(b'\n');
    line
}

impl Lode {
    /// Hand `line`, already encoded, to lode, if lode is there to take it.
    pub(crate) fn send(&mut self, line: &[u8]) {
        if self.stream.is_none() {
            self.connect();
        }
        if self.stream.is_none() {
            return;
        }
        if self.pending.len() + line.len() > PENDING_MAX {
            self.skipped += 1;
            return;
        }
        self.pending.extend_from_slice(line);
        self.flush();
    }

    /// Write what lode will take now.
    pub(crate) fn flush(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            self.pending.clear();
            return;
        };
        while !self.pending.is_empty() {
            match stream.write(&self.pending) {
                Ok(0) => break,
                Ok(count) => {
                    let _ = self.pending.drain(..count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // lode went away: its next instance gets a new
                    // connection, and these lines' console copies.
                    self.stream = None;
                    self.pending.clear();
                    break;
                }
            }
        }
    }

    /// How soon init's loop must come back to [`Lode::flush`].
    pub(crate) fn next_flush(&self) -> Option<Duration> {
        (!self.pending.is_empty()).then_some(Duration::from_millis(20))
    }

    /// Connect to lode, at most once every [`RETRY`].
    fn connect(&mut self) {
        let now = Instant::now();
        if now < self.next_try {
            return;
        }
        self.next_try = now + RETRY;
        if let Ok(stream) = UnixStream::connect(SOCKET)
            && stream.set_nonblocking(true).is_ok()
        {
            self.stream = Some(stream);
            self.pending.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::Whose;
    use ferrix_svc::event::UnitId;

    /// The same bytes `ferrix_lode_core`'s test of the format decodes.
    #[test]
    fn a_line_is_lodes_wire_format() {
        let said = Said {
            whose: Whose {
                unit: UnitId(3),
                pid: 812,
                ids: (1000, 1000),
                stderr: true,
                invocation: Some("5a".repeat(16)),
            },
            line: "listening on :80 \u{1b}[1mnow\u{1b}[0m".to_owned(),
        };
        assert_eq!(
            encode(&said, "web.service", 4_000_000),
            b"L web.service 5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a 812 1000 1000 err 4000000 listening on :80 \x1b[1mnow\x1b[0m\n"
        );
    }

    #[test]
    fn an_invocation_id_is_32_hex_digits_and_new_each_time() {
        let one = invocation_id();
        assert_eq!(one.len(), 32);
        assert!(one.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(one, invocation_id());
    }
}
