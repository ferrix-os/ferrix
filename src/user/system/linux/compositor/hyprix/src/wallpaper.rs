//! The wallpaper the `wallpaper =` lines ask for (`compositor_config::
//! Wallpaper`, `docs/RUNTIME-SETTINGS.md` §3.3), kept running by the
//! compositor rather than by an `exec-once` line.
//!
//! The lines in force -- `hyprland.conf`'s, then the Settings app's
//! `settings.conf`, then any `hyprctl keyword wallpaper` since -- are handed
//! to the wallpaper client, `wallpaper --line LINE…`, which draws them. When
//! they change it is started again with the new ones, and the one before is
//! ended a moment later, once the new one has had time to draw over it, so a
//! change shows no bare background in between. With no line, no client: a
//! configuration that starts its own wallpaper with `exec-once` keeps doing
//! so.
//!
//! The compositor knows the client's process, which is what lets it draw
//! that client's background surfaces under the lock screen
//! (`hyprix:wallpaper_lock`) and nobody else's.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compositor_config::Config;

use crate::options::Options;

/// How long the wallpaper before a change is kept once the new one is
/// started.
pub const OVERLAP: Duration = Duration::from_secs(2);

/// The wallpaper client the compositor runs, and the ones it is ending.
#[derive(Debug, Default)]
pub struct Wallpapers {
    /// The lines the running client was given.
    lines: Vec<String>,
    /// The running client.
    pid: Option<u32>,
    /// Clients replaced by a newer one, and when each is to be ended.
    retiring: Vec<(u32, Instant)>,
}

impl Wallpapers {
    /// The running client's process, whose background surfaces are the
    /// compositor's wallpaper.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Whether `pid` is the wallpaper client's, the running one or one
    /// being ended.
    #[must_use]
    pub fn is_ours(&self, pid: i32) -> bool {
        u32::try_from(pid).is_ok_and(|pid| {
            self.pid == Some(pid) || self.retiring.iter().any(|(old, _)| *old == pid)
        })
    }

    /// Start, start again or stop the client as `config`'s lines now say.
    /// Gives whether anything changed.
    pub fn follow(
        &mut self,
        config: &Config,
        options: &Options,
        socket: &Path,
        report: &mut dyn FnMut(&str),
    ) -> bool {
        let lines: Vec<String> = config
            .wallpapers
            .iter()
            .map(compositor_config::Wallpaper::line)
            .collect();
        if lines == self.lines && (self.pid.is_some() || lines.is_empty()) {
            return false;
        }
        if let Some(old) = self.pid.take() {
            self.retiring.push((old, Instant::now() + OVERLAP));
        }
        self.lines.clone_from(&lines);
        if lines.is_empty() {
            report("hyprix: no wallpaper line; the wallpaper client is ended");
            self.end_now();
            return true;
        }
        let program = program(options);
        let command = command(&program, &lines);
        match crate::state::start(&command, socket, options.instance.as_deref()) {
            Ok(pid) => {
                report(&format!(
                    "hyprix: wallpaper {} started as {pid}: {}",
                    program.display(),
                    lines.join(" | ")
                ));
                self.pid = Some(pid);
            }
            Err(error) => {
                report(&format!(
                    "hyprix: wallpaper {} did not start: {error}",
                    program.display()
                ));
            }
        }
        true
    }

    /// End the clients whose overlap is over.
    pub fn retire(&mut self, now: Instant) {
        self.retiring.retain(|&(pid, when)| {
            if when > now {
                return true;
            }
            terminate(pid);
            false
        });
    }

    /// When the next retiring client is to be ended, for the loop's wait.
    #[must_use]
    pub fn next_retirement(&self) -> Option<Instant> {
        self.retiring.iter().map(|&(_, when)| when).min()
    }

    /// End every client now: as the compositor ends.
    pub fn end_now(&mut self) {
        for (pid, _) in std::mem::take(&mut self.retiring) {
            terminate(pid);
        }
        if let Some(pid) = self.pid.take() {
            terminate(pid);
        }
    }
}

/// The wallpaper client: `Options::wallpaper`, else `wallpaper` beside the
/// compositor's own program, else `wallpaper` on the path.
fn program(options: &Options) -> PathBuf {
    if let Some(program) = &options.wallpaper {
        return program.clone();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("wallpaper")))
        .filter(|beside| beside.exists())
        .unwrap_or_else(|| PathBuf::from("wallpaper"))
}

/// The command line `start` reads: the program and a `--line` for each
/// line, each quoted whole.
#[must_use]
pub fn command(program: &Path, lines: &[String]) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let mut command = quote(&program.to_string_lossy());
    for line in lines {
        command.push_str(" --line ");
        command.push_str(&quote(line));
    }
    command
}

/// `SIGTERM` to `pid`, which the loop then reaps.
fn terminate(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: a signal to a process this compositor started; a pid
        // that has gone already is answered ESRCH and changes nothing.
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line with spaces and a quote reaches the client as one argument.
    #[test]
    fn each_line_is_one_argument() {
        let lines = vec![
            ", picture /home/ada/My Pictures/it's.png, fit fill, colour ff1d2027".to_owned(),
            "DP-1, colour ff000000".to_owned(),
        ];
        let command = command(Path::new("/bin/wallpaper"), &lines);
        let words = crate::command::words(&command).expect("words");
        assert_eq!(
            words,
            [
                "/bin/wallpaper",
                "--line",
                ", picture /home/ada/My Pictures/it's.png, fit fill, colour ff1d2027",
                "--line",
                "DP-1, colour ff000000",
            ]
        );
    }
}
