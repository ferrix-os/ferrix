//! `COMMAND | lode cat [-t ID] [-p PRIO] [--journal] [--tee FILE] [--echo]`:
//! standard input into the log, a line a record -- through a stdout stream,
//! as `systemd-cat` sends it, or with `--journal` a native-protocol datagram
//! a line. `--tee FILE` also writes everything to FILE, made afresh 0644
//! under the umask, as `>` makes it, so a script that kept a log file keeps
//! it and a restarted one reopens it; `--echo` also to standard output.
//!
//! Under load the least severe goes first (`docs/LOGGING.md` §7.5), at both
//! of its sinks, and nothing waits on either:
//!
//! * **To lode**: a line lode cannot take now waits in a queue of at most
//!   [`PENDING_MOST`] bytes; a full queue drops its oldest line of the least
//!   severe level it holds, and once lode takes lines again the first says
//!   what was dropped, `N lines dropped (debug: a, ...) from ID in T s while
//!   lode was behind`, at the level of the most severe dropped. The command's
//!   output keeps being read: it never waits for lode.
//! * **To standard output** -- the `--echo` copy, and every line while lode
//!   is not there -- through a console shed (`ferrix_log_shed::Caps::
//!   CONSOLE`): whatever reads it is a console a person reads, printed as it
//!   comes, so a debug flood's lines are shed first and said once a second.
//!
//! A lode that is not there, or goes away, is tried again each second.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use ferrix_log_shed::{Caps, Said, Shed, UNKNOWN, describe, level_of};

use crate::cli::{self, complain, usage};
use crate::sys;

/// The most bytes waiting for lode.
pub(crate) const PENDING_MOST: usize = 4 << 20;

/// How often a missing lode is looked for.
const RECONNECT_US: u64 = 1_000_000;

/// How long a pending queue may take to drain at the end.
const DRAIN_US: u64 = 5_000_000;

/// The lines waiting for lode.
#[derive(Default)]
struct Pending {
    /// Each line's level and bytes, its newline included.
    lines: VecDeque<(u8, Vec<u8>)>,
    /// Their bytes.
    bytes: usize,
    /// How much of the first line was sent.
    front_sent: usize,
    /// What was dropped since lode last took a line, by level.
    dropped: [u64; 8],
    /// When the first of those was dropped.
    since_us: u64,
}

impl Pending {
    /// Queue `line` at `level`, dropping the oldest of the least severe
    /// lines first while it would not fit.
    fn push(&mut self, level: u8, line: Vec<u8>, now_us: u64) {
        while self.bytes + line.len() > PENDING_MOST && self.evict(now_us) {}
        if self.bytes + line.len() > PENDING_MOST {
            self.count(level, now_us);
            return;
        }
        self.bytes += line.len();
        self.lines.push_back((level, line));
    }

    /// Count one line of `level` dropped at `now_us`.
    fn count(&mut self, level: u8, now_us: u64) {
        if self.dropped.iter().all(|count| *count == 0) {
            self.since_us = now_us;
        }
        if let Some(count) = self.dropped.get_mut(usize::from(level.min(7))) {
            *count += 1;
        }
    }

    /// Drop the oldest line of the least severe level queued, but not one
    /// partly sent: whether one went.
    fn evict(&mut self, now_us: u64) -> bool {
        let skip = usize::from(self.front_sent > 0);
        let Some(least) = self.lines.iter().skip(skip).map(|(level, _)| *level).max() else {
            return false;
        };
        let Some(at) = self
            .lines
            .iter()
            .skip(skip)
            .position(|(level, _)| *level == least)
        else {
            return false;
        };
        if let Some((level, line)) = self.lines.remove(at + skip) {
            self.bytes -= line.len();
            self.count(level, now_us);
        }
        true
    }

    /// Send what lode takes now, first the line saying what was dropped:
    /// `Ok(true)` once nothing waits, `Err` when lode has gone.
    fn flush(&mut self, fd: usize, identifier: &str, now_us: u64) -> Result<bool, ()> {
        if self.front_sent == 0 && self.dropped.iter().any(|count| *count > 0) {
            let said = Said {
                shed: self.dropped,
                span_us: now_us.saturating_sub(self.since_us),
            };
            let mut text = String::new();
            let _ = describe(&said, identifier, &mut text);
            let line = alloc::format!("<{}>lode cat: {text} while lode was behind\n", said.level());
            self.bytes += line.len();
            self.lines.push_front((said.level(), line.into_bytes()));
            self.dropped = [0; 8];
        }
        while let Some((_, line)) = self.lines.front() {
            let rest = line.get(self.front_sent..).unwrap_or_default();
            let sent = sys::send_some(fd, rest).map_err(|_| ())?;
            if sent == 0 {
                return Ok(false);
            }
            self.front_sent += sent;
            if self.front_sent >= line.len() {
                if let Some((_, line)) = self.lines.pop_front() {
                    self.bytes -= line.len();
                }
                self.front_sent = 0;
            }
        }
        Ok(true)
    }
}

/// The options, read.
struct Options {
    identifier: String,
    priority: Option<u8>,
    journal: bool,
    tee: Option<usize>,
    echo: bool,
}

/// Read the options; the exit status when they do not read.
fn options(words: &[String]) -> Result<Options, i32> {
    let mut options = Options {
        identifier: String::from("lode-cat"),
        // No `-p`: no level, which the log shows as unspecified.
        priority: None,
        journal: false,
        tee: None,
        echo: false,
    };
    let mut at = 0;
    while let Some(word) = words.get(at) {
        at += 1;
        match word.as_str() {
            "-t" | "--identifier" => match words.get(at) {
                Some(value) => {
                    options.identifier.clone_from(value);
                    at += 1;
                }
                None => return Err(usage("cat -t needs an identifier")),
            },
            "-p" | "--priority" => match words
                .get(at)
                .and_then(|value| ferrix_lode_core::record::level(value))
            {
                Some(value) if value != ferrix_lode_core::record::UNSPECIFIED => {
                    options.priority = Some(value);
                    at += 1;
                }
                _ => return Err(usage("cat -p needs a priority")),
            },
            "--journal" => options.journal = true,
            "--echo" => options.echo = true,
            "--tee" => match words.get(at) {
                Some(path) => {
                    at += 1;
                    // 0644 under the umask, as a shell's `>` makes it: a
                    // file of mode 0 could not be opened again.
                    match sys::openat(
                        sys::AT_FDCWD,
                        path.as_bytes(),
                        sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
                        0o644,
                    ) {
                        Ok(fd) => options.tee = Some(fd),
                        Err(error) => {
                            complain(&alloc::format!("lode: cat: {path}: errno {}", error.0));
                        }
                    }
                }
                None => return Err(usage("cat --tee needs a file")),
            },
            other => {
                return Err(usage(&alloc::format!(
                    "cat: {other}: pipe a command's output into `lode cat`"
                )));
            }
        }
    }
    Ok(options)
}

/// A connection to lode for `options`, the stream non-blocking.
fn connect(options: &Options) -> Option<usize> {
    let fd = cli::open_cat(
        options.journal,
        &options.identifier,
        options.priority,
        options.echo,
    )?;
    if !options.journal {
        let _ = sys::set_nonblocking(fd);
    }
    Some(fd)
}

/// `lode cat`; the exit status.
pub(crate) fn cat(words: &[String]) -> i32 {
    let options = match options(words) {
        Ok(options) => options,
        Err(status) => return status,
    };
    let now = || sys::clock_us(sys::CLOCK_MONOTONIC);
    let mut lode = connect(&options);
    let mut tried_us = now();
    let mut pending = Pending::default();
    let mut console = Shed::new(Caps::CONSOLE, now());
    let mut held: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 8192];
    // The command's output is read without waiting, so a slow lode or
    // console never holds the command up.
    let _ = sys::set_nonblocking(0);
    loop {
        let at = now();
        if lode.is_none() && at.saturating_sub(tried_us) >= RECONNECT_US {
            lode = connect(&options);
            tried_us = at;
        }
        if let Some(fd) = lode
            && !options.journal
            && pending.flush(fd, &options.identifier, at).is_err()
        {
            // lode went away: what follows goes to standard output, and
            // lode is looked for again.
            sys::close(fd);
            lode = None;
            tried_us = at;
        }
        if let Some(said) = console.said(at) {
            say_shed(&said, &options.identifier);
        }
        let got = match sys::read(0, &mut chunk) {
            Ok(got) => Some(got),
            Err(error) if error.0 == sys::EAGAIN => None,
            Err(_) => Some(0),
        };
        let Some(got) = got else {
            // Nothing to read now: wait for the command, briefly while
            // lines wait for lode.
            let wait = if pending.lines.is_empty() && lode.is_some() {
                1000
            } else {
                10
            };
            sys::wait_any(&[0], wait);
            continue;
        };
        let piece = chunk.get(..got).unwrap_or_default();
        if let Some(fd) = options.tee {
            let _ = sys::write_all(fd, piece);
        }
        held.extend_from_slice(piece);
        let finished = got == 0;
        let take = if finished {
            held.len()
        } else {
            held.iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |end| end + 1)
        };
        let lines: Vec<u8> = held.drain(..take).collect();
        for line in lines.split_inclusive(|&byte| byte == b'\n') {
            let level = options
                .priority
                .or_else(|| level_of(line))
                .unwrap_or(UNKNOWN);
            if (options.echo || lode.is_none()) && console.admit(level, at) {
                let _ = sys::write_all(1, line);
            }
            take_line(&options, &mut lode, &mut pending, level, line, at);
        }
        if finished {
            break;
        }
    }
    drain(&options, lode, &mut pending, now);
    if let Some(said) = console.last(now()) {
        say_shed(&said, &options.identifier);
    }
    0
}

/// One whole `line` of `level`, to lode: queued for the stream, sent as a
/// datagram for `--journal`.
fn take_line(
    options: &Options,
    lode: &mut Option<usize>,
    pending: &mut Pending,
    level: u8,
    line: &[u8],
    now_us: u64,
) {
    let Some(fd) = *lode else {
        return;
    };
    if options.journal {
        let _ = cli::send_lines(
            fd,
            true,
            &options.identifier,
            options.priority,
            line.to_vec(),
        );
        return;
    }
    let mut line = cli::info_said(options.priority, line.to_vec());
    if line.last() != Some(&b'\n') {
        line.push(b'\n');
    }
    pending.push(level, line, now_us);
}

/// At the end: what waits for lode sent, waiting for it at most
/// [`DRAIN_US`].
fn drain(options: &Options, lode: Option<usize>, pending: &mut Pending, now: impl Fn() -> u64) {
    let Some(fd) = lode else {
        return;
    };
    let until = now().saturating_add(DRAIN_US);
    while !options.journal && now() < until {
        match pending.flush(fd, &options.identifier, now()) {
            Ok(false) => sys::sleep_ms(10),
            Ok(true) | Err(()) => break,
        }
    }
    sys::close(fd);
}

/// Say on standard output what the console shed shed.
fn say_shed(said: &Said, identifier: &str) {
    let mut text = String::new();
    let _ = describe(said, identifier, &mut text);
    let line = alloc::format!(
        "<{}>lode cat: {text} to standard output; lode kept them\n",
        said.level()
    );
    let _ = sys::write_all(1, line.as_bytes());
}
