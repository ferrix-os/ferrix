//! Readiness (§5.3): the pipes `Type=notify` services write to, and the
//! main process of a `Type=forking` one.
//!
//! A notify service's pipe is open for as long as anything in the service
//! holds its write end, and each `KEY=value` line on it becomes an event:
//! `READY=1` readiness, `STATUS=` what `svc status` shows, `MAINPID=` the
//! main process, `WATCHDOG=` and `WATCHDOG_USEC=` the service watchdog's
//! pings (`docs/WATCHDOG-STAGES.md` §5.2).
//!
//! A forking service is up when its first process exits 0. Its main process
//! is then the one its `PIDFile=` names, or, with no such file, the one
//! process left in its cgroup; with neither, the manager keeps it running
//! until its cgroup empties, as systemd does with `GuessMainPID=`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use ferrix_svc::event::{Event, Pid, Ping, UnitId};
use ferrix_svc_proto::notify::{self, Lines, Notice};

/// One notify pipe.
#[derive(Debug)]
struct Pipe {
    unit: UnitId,
    file: File,
    lines: Lines,
}

/// The notify pipes being read.
#[derive(Debug, Default)]
pub(crate) struct Readiness {
    pipes: BTreeMap<u32, Pipe>,
    next: u32,
}

impl Readiness {
    /// Start reading `pipe` for `unit`; returns its number and descriptor.
    pub(crate) fn add(&mut self, unit: UnitId, pipe: OwnedFd) -> (u32, RawFd) {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let fd = pipe.as_raw_fd();
        let _ = self.pipes.insert(
            id,
            Pipe {
                unit,
                file: File::from(pipe),
                lines: Lines::new(),
            },
        );
        (id, fd)
    }

    /// Read pipe `id`; the events its lines are, and the descriptor to stop
    /// watching when it has closed.
    pub(crate) fn read(&mut self, id: u32) -> (Vec<Event>, Option<RawFd>) {
        let mut events = Vec::new();
        let Some(pipe) = self.pipes.get_mut(&id) else {
            return (events, None);
        };
        let unit = pipe.unit;
        let mut lines = Vec::new();
        let mut buffer = [0_u8; 1024];
        let mut closed = false;
        loop {
            match pipe.file.read(&mut buffer) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(count) => lines.extend(pipe.lines.push(buffer.get(..count).unwrap_or_default())),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        let mut done = None;
        if closed && let Some(mut pipe) = self.pipes.remove(&id) {
            lines.extend(pipe.lines.finish());
            done = Some(pipe.file.as_raw_fd());
        }
        for line in lines {
            match notify::parse(&line) {
                Notice::Ready => events.push(Event::Ready { unit, status: None }),
                Notice::Status(status) => events.push(Event::Status { unit, status }),
                Notice::MainPid(pid) => events.push(Event::MainPid {
                    unit,
                    pid: Pid(pid),
                }),
                Notice::Watchdog => events.push(Event::Watchdog {
                    unit,
                    ping: Ping::Alive,
                }),
                Notice::WatchdogTrigger => events.push(Event::Watchdog {
                    unit,
                    ping: Ping::Trigger,
                }),
                Notice::WatchdogUsec(micros) => events.push(Event::Watchdog {
                    unit,
                    ping: Ping::Interval(std::time::Duration::from_micros(micros)),
                }),
                Notice::Stopping | Notice::Reloading | Notice::Other => {}
            }
        }
        (events, done)
    }
}

/// A forking service's main process, once its first process has exited 0:
/// the pid in `pid_file`, if it names a live process, or else the one
/// process in `procs`.
pub(crate) fn forked_main(pid_file: Option<&str>, procs: &[u32]) -> Option<u32> {
    let from_file = pid_file
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 1 && fs::metadata(format!("/proc/{pid}")).is_ok());
    from_file.or(match procs {
        [only] => Some(*only),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::forked_main;

    #[test]
    fn with_no_pid_file_the_one_process_left_is_the_main_one() {
        assert_eq!(forked_main(None, &[42]), Some(42));
        assert_eq!(forked_main(None, &[42, 43]), None);
        assert_eq!(forked_main(Some("/nonexistent/pid"), &[]), None);
    }
}
