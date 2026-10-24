//! The log (§10): what services write to standard output and error.
//!
//! A stream whose `StandardOutput=` or `StandardError=` is `log` -- the
//! default for a service not on a terminal -- is a pipe init reads, one for
//! standard output and one for standard error. Each line goes to the
//! console behind the unit's name and the writer's pid, into a ring of the
//! unit's last [`RING`] lines, which `svc log` reads when lode cannot
//! answer, and to lode with who wrote it ([`crate::lode`]). The pipe stays
//! open for as long as anything holds its write end, so a daemon's children
//! log to it too.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use ferrix_svc::event::UnitId;
use ferrix_svc_proto::notify::Lines;

/// How many lines a unit's ring keeps.
pub(crate) const RING: usize = 256;

/// Whose a log pipe is: what init knows of the spawn it made it for.
#[derive(Debug, Clone)]
pub(crate) struct Whose {
    /// The unit.
    pub(crate) unit: UnitId,
    /// The process init started.
    pub(crate) pid: u32,
    /// Its user and group.
    pub(crate) ids: (u32, u32),
    /// Standard error's pipe, not standard output's.
    pub(crate) stderr: bool,
    /// The unit's run, 32 hex digits.
    pub(crate) invocation: Option<String>,
}

/// One pipe being read.
#[derive(Debug)]
struct Pipe {
    whose: Whose,
    file: File,
    lines: Lines,
}

/// Every log pipe, and every unit's ring.
#[derive(Debug, Default)]
pub(crate) struct Logs {
    pipes: BTreeMap<u32, Pipe>,
    rings: BTreeMap<UnitId, VecDeque<String>>,
    next: u32,
}

/// A line to say, as the unit's.
#[derive(Debug)]
pub(crate) struct Said {
    /// Whose pipe it came through.
    pub(crate) whose: Whose,
    /// The line.
    pub(crate) line: String,
}

impl Logs {
    /// Start reading `pipe`, the read end of a spawn's log; returns its
    /// number, for the caller's epoll token, and its descriptor.
    pub(crate) fn add(&mut self, whose: Whose, pipe: OwnedFd) -> (u32, RawFd) {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let fd = pipe.as_raw_fd();
        let _ = self.pipes.insert(
            id,
            Pipe {
                whose,
                file: File::from(pipe),
                lines: Lines::new(),
            },
        );
        (id, fd)
    }

    /// Read what pipe `id` holds. Returns the lines, and the descriptor to
    /// stop watching when the pipe has closed.
    pub(crate) fn read(&mut self, id: u32) -> (Vec<Said>, Option<RawFd>) {
        let mut said = Vec::new();
        let Some(pipe) = self.pipes.get_mut(&id) else {
            return (said, None);
        };
        let mut buffer = [0_u8; 4096];
        let mut closed = false;
        loop {
            match pipe.file.read(&mut buffer) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(count) => {
                    for line in pipe.lines.push(buffer.get(..count).unwrap_or_default()) {
                        said.push(Said {
                            whose: pipe.whose.clone(),
                            line,
                        });
                    }
                }
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
            if let Some(line) = pipe.lines.finish() {
                said.push(Said {
                    whose: pipe.whose.clone(),
                    line,
                });
            }
            done = Some(pipe.file.as_raw_fd());
            // The file closes here, after the caller has been given its
            // number to stop watching; epoll forgets a closed descriptor
            // anyway.
            drop(pipe);
        }
        for line in &said {
            let ring = self.rings.entry(line.whose.unit).or_default();
            if ring.len() == RING {
                let _ = ring.pop_front();
            }
            ring.push_back(format!("[{}] {}", line.whose.pid, line.line));
        }
        (said, done)
    }

    /// A unit's last `count` lines, oldest first.
    pub(crate) fn tail(&self, unit: UnitId, count: usize) -> Vec<String> {
        let Some(ring) = self.rings.get(&unit) else {
            return Vec::new();
        };
        ring.iter()
            .skip(ring.len().saturating_sub(count))
            .cloned()
            .collect()
    }
}
