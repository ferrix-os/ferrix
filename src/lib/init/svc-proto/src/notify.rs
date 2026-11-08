//! A service's readiness lines (`docs/INIT.md` §5.3).
//!
//! A `Type=notify` service writes `KEY=value` lines to the descriptor
//! `NotifyFd=` names: s6's readiness descriptor, with `sd_notify`'s words.
//! `READY=1` means ready; `STATUS=…` is shown by `svc status`; `MAINPID=`
//! names the main process when it is not the one init started; `WATCHDOG=1`,
//! `WATCHDOG=trigger` and `WATCHDOG_USEC=` drive the service watchdog
//! (`docs/WATCHDOG-STAGES.md` §5.2). Other keys
//! are kept for later and ignored today, as `sd_notify`'s receivers ignore
//! what they do not know. A line may arrive in pieces, so [`Lines`] keeps
//! the unfinished end until its newline comes.

use alloc::string::String;
use alloc::vec::Vec;

/// The longest line kept; a longer one is dropped whole, so a service
/// cannot make init hold more than this for it.
pub const MAX_LINE: usize = 4096;

/// What one line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// `READY=1`.
    Ready,
    /// `STATUS=text`.
    Status(String),
    /// `MAINPID=n`.
    MainPid(u32),
    /// `STOPPING=1`.
    Stopping,
    /// `RELOADING=1`.
    Reloading,
    /// `WATCHDOG=1`: alive.
    Watchdog,
    /// `WATCHDOG=trigger`: treat the watchdog as missed now.
    WatchdogTrigger,
    /// `WATCHDOG_USEC=n`: the interval for the rest of this run, in
    /// microseconds.
    WatchdogUsec(u64),
    /// Anything else, or a value that does not parse.
    Other,
}

/// What `line` says, without its newline.
pub fn parse(line: &str) -> Notice {
    let Some((key, value)) = line.split_once('=') else {
        return Notice::Other;
    };
    match key {
        "READY" if value == "1" => Notice::Ready,
        "STATUS" => Notice::Status(String::from(value)),
        "MAINPID" => value.parse().map_or(Notice::Other, Notice::MainPid),
        "STOPPING" if value == "1" => Notice::Stopping,
        "RELOADING" if value == "1" => Notice::Reloading,
        "WATCHDOG" if value == "1" => Notice::Watchdog,
        "WATCHDOG" if value == "trigger" => Notice::WatchdogTrigger,
        "WATCHDOG_USEC" => value.parse().map_or(Notice::Other, Notice::WatchdogUsec),
        _ => Notice::Other,
    }
}

/// Lines as they arrive in pieces.
#[derive(Debug, Default)]
pub struct Lines {
    partial: Vec<u8>,
    /// Whether the line being gathered went over [`MAX_LINE`].
    overlong: bool,
}

impl Lines {
    /// No bytes yet.
    pub fn new() -> Lines {
        Lines::default()
    }

    /// Bytes read; the whole lines they finish, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                if !self.overlong {
                    lines.push(String::from_utf8_lossy(&self.partial).into_owned());
                }
                self.partial.clear();
                self.overlong = false;
            } else if self.partial.len() < MAX_LINE {
                self.partial.push(byte);
            } else {
                self.overlong = true;
            }
        }
        lines
    }

    /// The end of the stream: an unfinished last line counts as a line.
    pub fn finish(&mut self) -> Option<String> {
        let line = (!self.partial.is_empty() && !self.overlong)
            .then(|| String::from_utf8_lossy(&self.partial).into_owned());
        self.partial.clear();
        self.overlong = false;
        line
    }
}
