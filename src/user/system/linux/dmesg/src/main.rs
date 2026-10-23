//! `dmesg`: the kernel log, as util-linux's `dmesg` shows it (the library's
//! documentation, `src/lib.rs`, says what and how).
//!
//! A command line it does not take is the usage and status 1, as
//! util-linux's; a failure is one line, `dmesg: ` and why, on standard
//! error, and status 1.

mod sys;

use std::io::{self, Write as _};
use std::process::ExitCode;

use dmesg::args::{self, Action, Follow, Options, USAGE, VERSION};
use dmesg::record::{self, Record};
use dmesg::show::{self, Human};

use sys::action;

/// The most one `/dev/kmsg` record takes: the kernel's `PRINTK_MESSAGE_MAX`.
const RECORD_ROOM: usize = 2048;

/// The buffer `syslog(2)` is read into when it will not say its size.
const SYSLOG_ROOM: usize = 1 << 17;

fn main() -> ExitCode {
    sys::die_on_broken_pipe();
    let options = match args::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            let _ = writeln!(io::stderr(), "dmesg: {error}\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "dmesg: {error}");
            ExitCode::from(1)
        }
    }
}

/// Do what `options` say.
fn run(options: &Options) -> Result<(), String> {
    let control = |what: i32, len: i32| {
        sys::klogctl(what, len)
            .map(|_| ())
            .map_err(|error| format!("klogctl failed: {error}"))
    };
    match options.action {
        Action::Help => return say(USAGE),
        Action::Version => return say(&format!("{VERSION}\n")),
        Action::Clear => return control(action::CLEAR, 0),
        Action::ConsoleOff => return control(action::CONSOLE_OFF, 0),
        Action::ConsoleOn => return control(action::CONSOLE_ON, 0),
        Action::ConsoleLevel(level) => return control(action::CONSOLE_LEVEL, i32::from(level)),
        Action::Print { .. } => {}
    }
    let kmsg = if options.syslog {
        None
    } else {
        sys::open_kmsg().ok()
    };
    match kmsg {
        Some(fd) => print_kmsg(options, &fd)?,
        None if options.follow != Follow::No => {
            return Err("cannot follow without /dev/kmsg".to_owned());
        }
        None => print_syslog(options)?,
    }
    // `-c` read through `/dev/kmsg` clears as util-linux's does, afterwards
    // through `syslog(2)`; through `syslog(2)` the read cleared already.
    if options.action == (Action::Print { clear: true }) && !options.syslog {
        control(action::CLEAR, 0)?;
    }
    Ok(())
}

/// Write `text` to standard output.
fn say(text: &str) -> Result<(), String> {
    io::stdout()
        .write_all(text.as_bytes())
        .map_err(|error| format!("write failed: {error}"))
}

/// Show the records `options` keep, to standard output, flushed after
/// `records` when following.
fn show(options: &Options, records: &[Record], human: &mut Human) -> Result<(), String> {
    let mut out = Vec::new();
    for record in records
        .iter()
        .filter(|record| options.keeps(record.priority))
    {
        show::line(record, options, &sys::Clocks, human, &mut out);
    }
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&out)
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("write failed: {error}"))
}

/// Read the log through `syslog(2)`, all of it at once.
fn print_syslog(options: &Options) -> Result<(), String> {
    let size = sys::klogctl(action::SIZE_BUFFER, 0).unwrap_or(SYSLOG_ROOM);
    let mut buf = vec![0u8; size.max(1)];
    let what = if options.action == (Action::Print { clear: true }) {
        action::READ_CLEAR
    } else {
        action::READ_ALL
    };
    let len = sys::klogctl_read(what, &mut buf)
        .map_err(|error| format!("read kernel buffer failed: {error}"))?;
    let records = record::parse_syslog(buf.get(..len).unwrap_or_default());
    show(options, &records, &mut Human::default())
}

/// Read the log through `/dev/kmsg`, a record a read: from the last clear,
/// or the end with `-W`, to the last record, and on as records come with
/// `-w` and `-W`.
fn print_kmsg(options: &Options, fd: &std::os::fd::OwnedFd) -> Result<(), String> {
    let whence = if options.follow == Follow::New {
        sys::SEEK_END
    } else {
        sys::SEEK_DATA
    };
    // Where seeking is refused, reading from the first record kept is the
    // next best thing.
    let _ = sys::seek(fd, whence);
    let mut human = Human::default();
    let mut buf = vec![0u8; RECORD_ROOM];
    let mut blocking = false;
    let mut dropped = record::Dropped::default();
    loop {
        match sys::read(fd, &mut buf) {
            Ok(0) => break,
            Ok(len) => {
                let read = buf.get(..len).unwrap_or_default();
                if let Some(record) = record::parse_kmsg(read) {
                    let note = dropped.before(&record);
                    let shown: Vec<Record> = note.into_iter().chain([record]).collect();
                    show(options, &shown, &mut human)?;
                }
            }
            Err(error) => match error.raw_os_error() {
                // Records gone before they were read: said before the next.
                Some(libc::EPIPE) => dropped.lost(),
                Some(libc::EINTR) => {}
                Some(libc::EAGAIN) if options.follow != Follow::No && !blocking => {
                    sys::set_blocking(fd)
                        .map_err(|error| format!("cannot wait on /dev/kmsg: {error}"))?;
                    blocking = true;
                }
                Some(libc::EAGAIN) => break,
                _ => return Err(format!("read kernel buffer failed: {error}")),
            },
        }
    }
    Ok(())
}
